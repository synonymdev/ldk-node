// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Compose LSPS with an optional bounded FFOR transport. The builder leaves FFOR disabled.

pub(crate) mod ffor;
#[cfg(test)]
mod tests;

use std::ops::Deref;
use std::sync::Arc;

use bitcoin::secp256k1::PublicKey;
use lightning::io;
use lightning::ln::msgs::{DecodeError, ErrorAction, Init, LightningError};
use lightning::ln::peer_handler::CustomMessageHandler;
use lightning::ln::wire::{CustomMessageReader, Type};
use lightning::util::logger::Logger;
use lightning::util::ser::{LengthLimitedRead, Writeable, Writer};
use lightning_liquidity::lsps0::ser::RawLSPSMessage;
use lightning_types::features::{InitFeatures, NodeFeatures};

use crate::liquidity::LiquiditySource;
use crate::types::LiquidityManager;
use ffor::setup::FforSetupAdapter;
use ffor::{FforFrame, FforReceiverTransport};

/// Independent LSPS and connection-scoped FFOR messages retain their own queue ordering.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum NodeCustomMessage {
	Liquidity(RawLSPSMessage),
	Ffor(FforFrame),
}

impl Type for NodeCustomMessage {
	fn type_id(&self) -> u16 {
		match self {
			Self::Liquidity(message) => message.type_id(),
			Self::Ffor(message) => message.type_id(),
		}
	}
}

impl Writeable for NodeCustomMessage {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), io::Error> {
		match self {
			Self::Liquidity(message) => message.write(writer),
			Self::Ffor(message) => message.write(writer),
		}
	}
}

pub(crate) struct NodeCustomMessageHandler<H = Arc<LiquidityManager>>
where
	H: Deref,
	H::Target: CustomMessageHandler<CustomMessage = RawLSPSMessage>,
{
	liquidity: Option<H>,
	ffor: Option<Arc<FforReceiverTransport>>,
	ffor_setup: Option<Arc<FforSetupAdapter>>,
}

impl NodeCustomMessageHandler {
	pub(crate) fn new_liquidity<L: Deref>(liquidity_source: Arc<LiquiditySource<L>>) -> Self
	where
		L::Target: Logger,
	{
		Self::new_liquidity_handler(liquidity_source.liquidity_manager())
	}
}

impl<H> NodeCustomMessageHandler<H>
where
	H: Deref,
	H::Target: CustomMessageHandler<CustomMessage = RawLSPSMessage>,
{
	fn new_liquidity_handler(liquidity: H) -> Self {
		Self { liquidity: Some(liquidity), ffor: None, ffor_setup: None }
	}

	pub(crate) fn new_ignoring() -> Self {
		Self { liquidity: None, ffor: None, ffor_setup: None }
	}

	// No production caller enables this seam until a native receiver owns admission and recovery.
	#[allow(dead_code)]
	pub(crate) fn with_ffor_receiver(mut self, receiver: Arc<FforReceiverTransport>) -> Self {
		self.ffor = Some(receiver);
		self.ffor_setup = None;
		self
	}

	// Explicit opt-in only. There is no builder setting, feature bit or invoice-facing API.
	#[allow(dead_code)]
	pub(crate) fn with_ffor_setup(mut self, setup: Arc<FforSetupAdapter>) -> Self {
		self.ffor = Some(Arc::clone(setup.transport()));
		self.ffor_setup = Some(setup);
		self
	}
}

impl<H> CustomMessageReader for NodeCustomMessageHandler<H>
where
	H: Deref,
	H::Target: CustomMessageHandler<CustomMessage = RawLSPSMessage>,
{
	type CustomMessage = NodeCustomMessage;

	fn read<RD: LengthLimitedRead>(
		&self, message_type: u16, buffer: &mut RD,
	) -> Result<Option<Self::CustomMessage>, DecodeError> {
		if self.ffor.is_some() && FforFrame::handles_type(message_type) {
			return FforFrame::read(message_type, buffer)
				.map(|message| Some(NodeCustomMessage::Ffor(message)));
		}
		match self.liquidity.as_ref() {
			Some(liquidity) => liquidity
				.read(message_type, buffer)
				.map(|message| message.map(NodeCustomMessage::Liquidity)),
			None => Ok(None),
		}
	}
}

impl<H> CustomMessageHandler for NodeCustomMessageHandler<H>
where
	H: Deref,
	H::Target: CustomMessageHandler<CustomMessage = RawLSPSMessage>,
{
	fn handle_custom_message(
		&self, message: NodeCustomMessage, sender: PublicKey,
	) -> Result<(), LightningError> {
		match message {
			NodeCustomMessage::Liquidity(message) => match self.liquidity.as_ref() {
				Some(liquidity) => liquidity.handle_custom_message(message, sender),
				None => Ok(()),
			},
			NodeCustomMessage::Ffor(message) => {
				if let Some(setup) = self.ffor_setup.as_ref() {
					if matches!(message.type_id(), 55003 | 55049) {
						return setup.handle(sender, &message).map(|_| ()).map_err(|error| {
							LightningError {
								err: format!("FFOR receiver setup: {}", error),
								// Do not process following HTLC frames after failed native admission.
								action: ErrorAction::DisconnectPeer { msg: None },
							}
						});
					}
				}
				match self.ffor.as_ref() {
					Some(ffor) => ffor.receive(sender, message),
					None => Ok(()),
				}
			},
		}
	}

	fn get_and_clear_pending_msg(&self) -> Vec<(PublicKey, NodeCustomMessage)> {
		let mut messages: Vec<_> = self.liquidity.as_ref().map_or_else(Vec::new, |liquidity| {
			liquidity
				.get_and_clear_pending_msg()
				.into_iter()
				.map(|(peer, message)| (peer, NodeCustomMessage::Liquidity(message)))
				.collect()
		});
		if let Some(ffor) = self.ffor.as_ref() {
			messages.extend(
				ffor.drain_outbound()
					.into_iter()
					.map(|(peer, message)| (peer, NodeCustomMessage::Ffor(message))),
			);
		}
		messages
	}

	fn provided_node_features(&self) -> NodeFeatures {
		self.liquidity
			.as_ref()
			.map_or_else(NodeFeatures::empty, |liquidity| liquidity.provided_node_features())
	}

	fn provided_init_features(&self, peer: PublicKey) -> InitFeatures {
		self.liquidity
			.as_ref()
			.map_or_else(InitFeatures::empty, |liquidity| liquidity.provided_init_features(peer))
	}

	fn peer_connected(&self, peer: PublicKey, init: &Init, inbound: bool) -> Result<(), ()> {
		if let Some(ffor) = self.ffor.as_ref() {
			// A failed replacement callback must not leave the previous connection usable.
			ffor.peer_disconnected(peer);
		}
		if let Some(liquidity) = self.liquidity.as_ref() {
			liquidity.peer_connected(peer, init, inbound)?;
		}
		if let Some(setup) = self.ffor_setup.as_ref() {
			setup.peer_connected(peer);
		} else if let Some(ffor) = self.ffor.as_ref() {
			// Exhausting optional FFOR capacity must not disconnect an ordinary peer.
			ffor.peer_connected(peer);
		}
		Ok(())
	}

	fn peer_disconnected(&self, peer: PublicKey) {
		if let Some(ffor) = self.ffor.as_ref() {
			ffor.peer_disconnected(peer);
		}
		if let Some(liquidity) = self.liquidity.as_ref() {
			liquidity.peer_disconnected(peer);
		}
	}
}

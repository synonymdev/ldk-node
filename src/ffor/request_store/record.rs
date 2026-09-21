use std::fmt;

use bitcoin::secp256k1::PublicKey;
use lightning::ln::ffor::{FFORReceiverId, FFORReceiverParameters};
use lightning::ln::types::ChannelId;
use lightning_ffor::amounts::FeePolicy;
use lightning_invoice::Description;
use zeroize::Zeroizing;

use super::{RequestStoreError, VERSION};

const MAX_CLIENT_ID_BYTES: usize = 128;

#[derive(Clone, PartialEq, Eq)]
pub(in crate::ffor) struct RequestIntent {
	client_id: String,
	amount_msat: u64,
	description: String,
}

impl fmt::Debug for RequestIntent {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("RequestIntent")
			.field("amount_msat", &self.amount_msat)
			.field("description_bytes", &self.description.len())
			.finish_non_exhaustive()
	}
}

impl RequestIntent {
	pub(in crate::ffor) fn new(
		client_id: String, amount_msat: u64, description: String,
	) -> Result<Self, RequestStoreError> {
		validate_client_id(&client_id)?;
		if amount_msat == 0 || Description::new(description.clone()).is_err() {
			return Err(RequestStoreError::InvalidIntent);
		}
		Ok(Self { client_id, amount_msat, description })
	}
	pub(in crate::ffor) fn client_id(&self) -> &str {
		&self.client_id
	}
	pub(in crate::ffor) fn amount_msat(&self) -> u64 {
		self.amount_msat
	}
	pub(in crate::ffor) fn description(&self) -> &str {
		&self.description
	}
}

pub(super) fn validate_client_id(id: &str) -> Result<(), RequestStoreError> {
	if id.is_empty() || id.len() > MAX_CLIENT_ID_BYTES || id.trim().is_empty() {
		Err(RequestStoreError::InvalidIntent)
	} else {
		Ok(())
	}
}

/// Exact local selection. It is not proof that this channel can admit the request.
#[derive(Clone, Debug)]
pub(in crate::ffor) struct RequestPlan {
	pub(in crate::ffor) channel: ChannelId,
	pub(in crate::ffor) settlement: PublicKey,
	pub(in crate::ffor) parameters: FFORReceiverParameters,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ffor) struct RetainedSelector {
	pub(in crate::ffor) channel: ChannelId,
	pub(in crate::ffor) epoch: [u8; 32],
}

impl RetainedSelector {
	pub(super) fn from_native(id: &FFORReceiverId) -> Self {
		Self { channel: id.channel_id(), epoch: id.epoch_id() }
	}
	pub(super) fn matches(&self, id: &FFORReceiverId) -> bool {
		*self == Self::from_native(id)
	}
}

/// Immutable application data plus an optional historical selector, never a native capability.
#[derive(Clone, Debug)]
pub(in crate::ffor) struct StoredRequest {
	intent: RequestIntent,
	plan: RequestPlan,
	selector: Option<RetainedSelector>,
}

impl StoredRequest {
	pub(super) fn new(intent: RequestIntent, plan: RequestPlan) -> Result<Self, RequestStoreError> {
		let record = Self { intent, plan, selector: None };
		record.validate()?;
		Ok(record)
	}
	pub(in crate::ffor) fn intent(&self) -> &RequestIntent {
		&self.intent
	}
	pub(in crate::ffor) fn plan(&self) -> &RequestPlan {
		&self.plan
	}
	pub(in crate::ffor) fn selector(&self) -> Option<RetainedSelector> {
		self.selector
	}
	pub(in crate::ffor) fn local_request_id(&self) -> [u8; 32] {
		self.plan.parameters.local_request_id
	}

	pub(super) fn same_request(&self, other: &Self) -> bool {
		let mut left = self.clone();
		let mut right = other.clone();
		left.selector = None;
		right.selector = None;
		left.encode() == right.encode()
	}

	pub(super) fn bind(&mut self, id: &FFORReceiverId) -> Result<bool, RequestStoreError> {
		if id.channel_id() != self.plan.channel {
			return Err(RequestStoreError::Conflict);
		}
		let selector = RetainedSelector::from_native(id);
		match self.selector {
			Some(existing) if existing != selector => Err(RequestStoreError::Conflict),
			Some(_) => Ok(false),
			None => {
				self.selector = Some(selector);
				Ok(true)
			},
		}
	}

	fn validate(&self) -> Result<(), RequestStoreError> {
		let p = &self.plan.parameters;
		if p.amounts_msat.as_slice() != [self.intent.amount_msat]
			|| p.minimum_payment_msat == 0
			|| p.minimum_payment_msat > self.intent.amount_msat
			|| p.settlement_deadline == 0
			|| p.claim_margin_blocks == 0
			|| p.voucher_expiry >= 500_000_000
			|| p.settlement_deadline
				.checked_add(p.claim_margin_blocks)
				.is_none_or(|height| height > p.voucher_expiry)
			|| self.selector.is_some_and(|selector| selector.channel != self.plan.channel)
		{
			return Err(RequestStoreError::InvalidIntent);
		}
		FeePolicy {
			base_msat: p.fee_base_msat,
			proportional_millionths: p.fee_proportional_millionths,
		}
		.gross_msat(self.intent.amount_msat)
		.map_err(|_| RequestStoreError::InvalidIntent)?;
		if let Some(witnesses) = &p.witness_peers {
			if witnesses.is_empty()
				|| witnesses.len() > 4
				|| witnesses.iter().enumerate().any(|(i, key)| witnesses[..i].contains(key))
			{
				return Err(RequestStoreError::InvalidIntent);
			}
		}
		Ok(())
	}

	pub(super) fn encode(&self) -> Zeroizing<Vec<u8>> {
		let mut out = Zeroizing::new(Vec::new());
		out.extend_from_slice(&VERSION.to_be_bytes());
		string(&mut out, &self.intent.client_id);
		out.extend_from_slice(&self.intent.amount_msat.to_be_bytes());
		string(&mut out, &self.intent.description);
		out.extend_from_slice(&self.plan.channel.0);
		out.extend_from_slice(&self.plan.settlement.serialize());
		let p = &self.plan.parameters;
		out.extend_from_slice(&p.local_request_id);
		out.extend_from_slice(&p.minimum_payment_msat.to_be_bytes());
		for value in [
			p.settlement_deadline,
			p.voucher_expiry,
			p.fee_base_msat,
			p.fee_proportional_millionths,
			p.claim_margin_blocks,
		] {
			out.extend_from_slice(&value.to_be_bytes());
		}
		match &p.witness_peers {
			None => out.push(0),
			Some(peers) => {
				out.push(peers.len() as u8);
				for key in peers {
					out.extend_from_slice(&key.serialize());
				}
			},
		}
		out.push(u8::from(p.hash_chain));
		match self.selector {
			None => out.push(0),
			Some(selector) => {
				out.push(1);
				out.extend_from_slice(&selector.epoch);
			},
		}
		out
	}

	pub(super) fn decode(bytes: &[u8]) -> Result<Self, RequestStoreError> {
		let mut reader = Reader(bytes);
		if u16::from_be_bytes(reader.array()?) != VERSION {
			return Err(RequestStoreError::Corrupt);
		}
		let client_id = reader.string(MAX_CLIENT_ID_BYTES)?;
		let amount = u64::from_be_bytes(reader.array()?);
		let intent = RequestIntent::new(client_id, amount, reader.string(639)?)?;
		let channel = ChannelId(reader.array()?);
		let settlement = reader.key()?;
		let mut parameters = FFORReceiverParameters {
			local_request_id: reader.array()?,
			amounts_msat: vec![amount],
			minimum_payment_msat: u64::from_be_bytes(reader.array()?),
			settlement_deadline: reader.u32()?,
			voucher_expiry: reader.u32()?,
			fee_base_msat: reader.u32()?,
			fee_proportional_millionths: reader.u32()?,
			claim_margin_blocks: reader.u32()?,
			witness_peers: None,
			hash_chain: false,
		};
		match reader.byte()? {
			0 => {},
			count @ 1..=4 => {
				parameters.witness_peers =
					Some((0..count).map(|_| reader.key()).collect::<Result<_, _>>()?)
			},
			_ => return Err(RequestStoreError::Corrupt),
		}
		parameters.hash_chain = match reader.byte()? {
			0 => false,
			1 => true,
			_ => return Err(RequestStoreError::Corrupt),
		};
		let selector = match reader.byte()? {
			0 => None,
			1 => Some(RetainedSelector { channel, epoch: reader.array()? }),
			_ => return Err(RequestStoreError::Corrupt),
		};
		if !reader.0.is_empty() {
			return Err(RequestStoreError::Corrupt);
		}
		let record =
			Self { intent, plan: RequestPlan { channel, settlement, parameters }, selector };
		record.validate()?;
		Ok(record)
	}
}

fn string(out: &mut Vec<u8>, value: &str) {
	out.extend_from_slice(&(value.len() as u16).to_be_bytes());
	out.extend_from_slice(value.as_bytes());
}

struct Reader<'a>(&'a [u8]);
impl<'a> Reader<'a> {
	fn take(&mut self, count: usize) -> Result<&'a [u8], RequestStoreError> {
		let value = self.0.get(..count).ok_or(RequestStoreError::Corrupt)?;
		self.0 = &self.0[count..];
		Ok(value)
	}
	fn array<const N: usize>(&mut self) -> Result<[u8; N], RequestStoreError> {
		self.take(N)?.try_into().map_err(|_| RequestStoreError::Corrupt)
	}
	fn byte(&mut self) -> Result<u8, RequestStoreError> {
		Ok(self.array::<1>()?[0])
	}
	fn u32(&mut self) -> Result<u32, RequestStoreError> {
		Ok(u32::from_be_bytes(self.array()?))
	}
	fn key(&mut self) -> Result<PublicKey, RequestStoreError> {
		PublicKey::from_slice(self.take(33)?).map_err(|_| RequestStoreError::Corrupt)
	}
	fn string(&mut self, maximum: usize) -> Result<String, RequestStoreError> {
		let len = usize::from(u16::from_be_bytes(self.array()?));
		if len > maximum {
			return Err(RequestStoreError::Capacity);
		}
		String::from_utf8(self.take(len)?.to_vec()).map_err(|_| RequestStoreError::Corrupt)
	}
}

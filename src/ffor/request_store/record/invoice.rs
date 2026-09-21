//! Protected storage framing. These records are not native publication capabilities.

use std::fmt;

use bitcoin::hashes::{sha256, Hash};
use lightning::ln::channelmanager::PaymentId;
use lightning::util::ser::Writeable;
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescriptionRef};
use lightning_types::payment::PaymentHash;

use super::{string, Reader, RequestIntent, RequestStoreError};
use crate::payment::store::{PaymentDetails, PaymentDirection, PaymentKind, PaymentStatus};

pub(in crate::ffor::request_store) const MAX_INVOICE_BYTES: usize = 4096;

/// Fixed application expiry policy, retained before asking native to assign invoice bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ffor) struct InvoicePolicy {
	pub(in crate::ffor) expiry_seconds: u32,
	pub(in crate::ffor) safety_margin_seconds: u32,
}

impl InvoicePolicy {
	pub(super) fn validate(&self) -> Result<(), RequestStoreError> {
		if self.expiry_seconds == 0 {
			return Err(RequestStoreError::InvalidIntent);
		}
		Ok(())
	}
}

#[derive(Clone, Debug)]
pub(super) enum InvoiceAllocation {
	Legacy,
	Reserved { policy: InvoicePolicy, invoice: Option<RetainedInvoice> },
}

#[derive(Clone, PartialEq, Eq)]
pub(in crate::ffor::request_store) struct RetainedInvoice {
	context_digest: [u8; 32],
	bolt11: String,
	payment_created_at: u64,
	payment_confirmation: Option<[u8; 32]>,
}

impl fmt::Debug for RetainedInvoice {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("RetainedInvoice")
			.field("invoice_bytes", &self.bolt11.len())
			.field("payment_confirmed", &self.payment_confirmation.is_some())
			.finish_non_exhaustive()
	}
}

impl RetainedInvoice {
	pub(in crate::ffor::request_store) fn new(
		context_digest: [u8; 32], bolt11: String, payment_created_at: u64,
	) -> Result<Self, RequestStoreError> {
		let value = Self { context_digest, bolt11, payment_created_at, payment_confirmation: None };
		value.parsed()?;
		Ok(value)
	}
	pub(in crate::ffor::request_store) fn context_digest(&self) -> [u8; 32] {
		self.context_digest
	}
	pub(in crate::ffor::request_store) fn invoice(&self) -> &str {
		&self.bolt11
	}
	pub(in crate::ffor::request_store) fn digest(&self) -> [u8; 32] {
		sha256::Hash::hash(self.bolt11.as_bytes()).to_byte_array()
	}
	pub(in crate::ffor::request_store) fn payment_confirmed(&self) -> bool {
		self.payment_confirmation.is_some()
	}
	pub(in crate::ffor::request_store) fn parsed(
		&self,
	) -> Result<Bolt11Invoice, RequestStoreError> {
		if self.bolt11.is_empty() || self.bolt11.len() > MAX_INVOICE_BYTES {
			return Err(RequestStoreError::Capacity);
		}
		let invoice: Bolt11Invoice = self.bolt11.parse().map_err(|_| RequestStoreError::Corrupt)?;
		if invoice.to_string() != self.bolt11 {
			return Err(RequestStoreError::Corrupt);
		}
		Ok(invoice)
	}
	pub(in crate::ffor::request_store) fn payment(
		&self,
	) -> Result<PaymentDetails, RequestStoreError> {
		let invoice = self.parsed()?;
		let description = match invoice.description() {
			Bolt11InvoiceDescriptionRef::Direct(value) => value.as_inner().0.clone(),
			_ => return Err(RequestStoreError::Conflict),
		};
		let hash = PaymentHash(invoice.payment_hash().to_byte_array());
		Ok(PaymentDetails {
			id: PaymentId(hash.0),
			kind: PaymentKind::Bolt11 {
				hash,
				preimage: None,
				secret: Some(*invoice.payment_secret()),
				description: Some(description),
				bolt11: Some(self.bolt11.clone()),
			},
			amount_msat: invoice.amount_milli_satoshis(),
			fee_paid_msat: None,
			direction: PaymentDirection::Inbound,
			status: PaymentStatus::Pending,
			latest_update_timestamp: self.payment_created_at,
		})
	}
	pub(in crate::ffor::request_store) fn confirm_payment(
		&mut self, payment: &PaymentDetails,
	) -> Result<bool, RequestStoreError> {
		if self.payment()? != *payment {
			return Err(RequestStoreError::Conflict);
		}
		let digest = sha256::Hash::hash(&payment.encode()).to_byte_array();
		match self.payment_confirmation {
			Some(existing) if existing != digest => Err(RequestStoreError::Conflict),
			Some(_) => Ok(false),
			None => {
				self.payment_confirmation = Some(digest);
				Ok(true)
			},
		}
	}
	pub(super) fn validate(
		&self, intent: &RequestIntent, policy: InvoicePolicy,
	) -> Result<(), RequestStoreError> {
		let invoice = self.parsed()?;
		if invoice.amount_milli_satoshis() != Some(intent.amount_msat())
			|| invoice.expiry_time().as_secs() == 0
			|| invoice.expiry_time().as_secs() > u64::from(policy.expiry_seconds)
			|| !matches!(invoice.description(), Bolt11InvoiceDescriptionRef::Direct(value) if value.as_inner().0 == intent.description())
		{
			return Err(RequestStoreError::Conflict);
		}
		let payment_digest = sha256::Hash::hash(&self.payment()?.encode()).to_byte_array();
		if self.payment_confirmation.is_some_and(|digest| digest != payment_digest) {
			return Err(RequestStoreError::Corrupt);
		}
		Ok(())
	}
}

impl InvoiceAllocation {
	pub(super) fn encode(&self, out: &mut Vec<u8>) {
		if let Self::Reserved { policy, invoice } = self {
			out.extend_from_slice(&policy.expiry_seconds.to_be_bytes());
			out.extend_from_slice(&policy.safety_margin_seconds.to_be_bytes());
			match invoice {
				None => out.push(0),
				Some(invoice) => {
					out.push(1);
					out.extend_from_slice(&invoice.context_digest);
					string(out, &invoice.bolt11);
					out.extend_from_slice(&invoice.payment_created_at.to_be_bytes());
					match invoice.payment_confirmation {
						None => out.push(0),
						Some(digest) => {
							out.push(1);
							out.extend_from_slice(&digest);
						},
					}
				},
			}
		}
	}
	pub(super) fn decode(reader: &mut Reader<'_>) -> Result<Self, RequestStoreError> {
		let policy =
			InvoicePolicy { expiry_seconds: reader.u32()?, safety_margin_seconds: reader.u32()? };
		policy.validate()?;
		let invoice = match reader.byte()? {
			0 => None,
			1 => Some(RetainedInvoice {
				context_digest: reader.array()?,
				bolt11: reader.string(MAX_INVOICE_BYTES)?,
				payment_created_at: u64::from_be_bytes(reader.array()?),
				payment_confirmation: match reader.byte()? {
					0 => None,
					1 => Some(reader.array()?),
					_ => return Err(RequestStoreError::Corrupt),
				},
			}),
			_ => return Err(RequestStoreError::Corrupt),
		};
		Ok(Self::Reserved { policy, invoice })
	}
}

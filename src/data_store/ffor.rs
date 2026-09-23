//! Private exact Pending payment confirmation and publication exclusion for FFOR.

use std::ops::Deref;

use lightning::util::persist::KVStoreSync;
use lightning::util::ser::Writeable;
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescriptionRef};

use super::{DataStore, StorableObjectId};
use crate::io::{
	PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE, PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
};
use crate::logger::LdkLogger;
use crate::payment::store::{PaymentDetails, PaymentDirection, PaymentKind, PaymentStatus};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FFORPaymentError {
	InvalidPending,
	Missing,
	Conflict,
	Terminal,
	Storage,
}

impl<L: Deref> DataStore<PaymentDetails, L>
where
	L::Target: LdkLogger,
{
	/// Serialize against ordinary payment updates and deletion. Visibility alone does not confirm
	/// durability: even an unchanged restored row is written successfully before returning.
	pub(crate) fn confirm_ffor_pending(
		&self, expected: &PaymentDetails, require_existing: bool,
	) -> Result<(), FFORPaymentError> {
		validate_pending(expected)?;
		if self.primary_namespace != PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE
			|| self.secondary_namespace != PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE
		{
			return Err(FFORPaymentError::Conflict);
		}
		let mut objects = self.objects.lock().unwrap();
		let existing = objects.get(&expected.id);
		if let Some(existing) = existing {
			check_current(existing, expected)?;
		} else if require_existing {
			return Err(FFORPaymentError::Missing);
		}
		let bytes = expected.encode();
		let key = expected.id.encode_to_hex_str();
		match KVStoreSync::read(
			&*self.kv_store,
			&self.primary_namespace,
			&self.secondary_namespace,
			&key,
		) {
			Ok(actual) if actual == bytes => {},
			Ok(_) => return Err(FFORPaymentError::Conflict),
			Err(error) if error.kind() == lightning::io::ErrorKind::NotFound => {
				if require_existing || existing.is_some() {
					return Err(FFORPaymentError::Missing);
				}
			},
			Err(_) => return Err(FFORPaymentError::Storage),
		}
		// Persist first. A failed or ambiguously visible write never installs an in-memory row.
		KVStoreSync::write(
			&*self.kv_store,
			&self.primary_namespace,
			&self.secondary_namespace,
			&key,
			bytes,
		)
		.map_err(|_| FFORPaymentError::Storage)?;
		objects.insert(expected.id, expected.clone());
		Ok(())
	}

	/// Exclude all ordinary payment mutations through the final native publication call. The
	/// caller already confirmed storage and the protected request before entering. Lock order is
	/// payment objects, then native; the supplied action must not reenter payment or request storage.
	pub(crate) fn with_ffor_pending<T>(
		&self, expected: &PaymentDetails, publish: impl FnOnce() -> T,
	) -> Result<T, FFORPaymentError> {
		validate_pending(expected)?;
		let objects = self.objects.lock().unwrap();
		check_current(objects.get(&expected.id).ok_or(FFORPaymentError::Missing)?, expected)?;
		let result = publish();
		drop(objects);
		Ok(result)
	}
}

fn check_current(
	current: &PaymentDetails, expected: &PaymentDetails,
) -> Result<(), FFORPaymentError> {
	if current.status != PaymentStatus::Pending {
		return Err(FFORPaymentError::Terminal);
	}
	if current != expected {
		return Err(FFORPaymentError::Conflict);
	}
	Ok(())
}

fn validate_pending(expected: &PaymentDetails) -> Result<(), FFORPaymentError> {
	let PaymentKind::Bolt11 {
		hash,
		preimage: None,
		secret: Some(secret),
		description: Some(description),
		bolt11: Some(wire),
	} = &expected.kind
	else {
		return Err(FFORPaymentError::InvalidPending);
	};
	if expected.direction != PaymentDirection::Inbound
		|| expected.status != PaymentStatus::Pending
		|| expected.id.0 != hash.0
		|| expected.fee_paid_msat.is_some()
		|| wire.len() > 4096
	{
		return Err(FFORPaymentError::InvalidPending);
	}
	let invoice: Bolt11Invoice = wire.parse().map_err(|_| FFORPaymentError::InvalidPending)?;
	if invoice.to_string() != *wire
		|| invoice.amount_milli_satoshis() != expected.amount_msat
		|| !expected.amount_msat.is_some_and(|amount| amount > 0)
		|| invoice.payment_hash().as_ref() != hash.0
		|| invoice.payment_secret() != secret
		|| !matches!(invoice.description(), Bolt11InvoiceDescriptionRef::Direct(value) if value.as_inner().0 == *description)
	{
		return Err(FFORPaymentError::InvalidPending);
	}
	Ok(())
}

#[cfg(test)]
mod tests;

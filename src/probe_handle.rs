// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Handle returned when sending pre-flight probes.
//!
//! UniFFI uses `dictionary ProbeHandle` in `bindings/ldk_node.udl`; this module defines the Rust
//! struct with the same fields. With `feature = "uniffi"`, `lib.rs` must `pub use` this type
//! **before** `uniffi::include_scaffolding!` so the generated `FfiConverter` impl applies here (same
//! pattern as [`crate::SpendableUtxo`], [`crate::PeerDetails`], etc.).

use lightning::ln::channelmanager::PaymentId;
use lightning_types::payment::PaymentHash;

/// Identifies one outbound probe; match against [`crate::Event::ProbeSuccessful`] /
/// [`crate::Event::ProbeFailed`] using [`Self::payment_id`] and/or [`Self::payment_hash`].
///
/// The [`PaymentHash`] is **not** the BOLT11 invoice payment hash; LDK generates a synthetic hash
/// per probe.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ProbeHandle {
	/// Synthetic probe payment hash (not the BOLT11 invoice payment hash).
	pub payment_hash: PaymentHash,
	/// Local id for this probe; matches probe [`crate::Event`] variants.
	pub payment_id: PaymentId,
}

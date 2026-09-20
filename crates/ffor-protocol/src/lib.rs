//! Pure primitives for the draft FFOR Variant D protocol.
//!
//! These primitives do not implement a receiver or settlement peer. In particular, successful
//! arithmetic or transcript construction is not proof of channel commitments, signature
//! verification, durable activation, or readiness to expose an offline invoice.
//!
//! The channel engine must retain sole ownership of commitment transitions and signing keys.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod amounts;
pub mod book;
pub mod transcript;

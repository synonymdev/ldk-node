//! Private witness recovery storage and bounded orchestration, not invoice authority.
//!
//! Only the opt-in `runtime` composes these owners, and only when the builder's offline-receive
//! configuration is set. Bindings come from opaque native historical contexts; provisioning requires current native authority checked under the manager's
//! transition locks. Retained acknowledgements and encrypted evidence confer no payment credit.

#[allow(dead_code)]
mod witness_store;

#[allow(dead_code)]
mod witness_owner;

#[allow(dead_code)]
mod request_store;

pub(crate) mod runtime;

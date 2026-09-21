//! Private witness recovery storage, not a native activation or invoice authority.
//!
//! No builder, transport, or payment provider constructs this module yet. Bindings come from opaque
//! native historical contexts; future registration and external work still require current native
//! authority checked under the manager's transition locks.

// This bounded storage boundary remains disconnected until native lifecycle ownership exists.
#[allow(dead_code)]
mod witness_store;

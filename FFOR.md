# Experimental offline receiving

The integration target is Bitkit's current rust-lightning 0.2.5 baseline and
Blocktank's LND v0.21.3-beta baseline. Work is tracked in
[issue 117](https://github.com/synonymdev/ldk-node/issues/117).

The shared protocol implementation lives in `lightning-ffor` in the
[Rust channel-engine draft](https://github.com/synonymdev/rust-lightning/pull/4).
Node pins an exact commit in Cargo.toml and Cargo.lock. Wire parsers, authenticated
setup derivation, transcript hashes, reference fixtures and fuzz tests belong to
that crate. Keeping one implementation lets Node and the channel engine use the
same validation rules.

This dependency does not enable offline receiving. The current native bindings
and ordinary invoice behavior remain unchanged. The mobile draft providers expose
no production capability until authenticated activation, durable channel ownership
and recovery are implemented together.

## Required runtime integration

- Bind signed setup to the actual local identity, peer, chain and channel limits.
- Park a complete voucher book outside ordinary invoice and forwarding handling.
- Freeze the verified commitment pair through the real quiescence protocol.
- Persist exact activation and acknowledgement evidence before releasing either
  acknowledgement or invoice readiness. Retain evidence after channel removal.
- Restore active epochs, reconcile reconnect state and fail setup safely after a
  disconnect, timeout, partial round or storage failure.
- Connect settlement, slot accounting, witness/mailbox recovery and on-chain
  protection before offering an offline invoice.
- Generate native bindings and connect the Android and iOS providers to the same
  exact-amount, single-channel eligibility and invoice metadata contract.

## Trust boundaries and validation

Peer messages, invoices, reconnect reports and local crash recovery are separate
inputs. A valid signature proves message authorship, not current channel state or
successful storage. Eligibility requires the complete native state transition,
not a parser result or cached liquidity estimate. Application code must not export
node keys or manually assert that a commitment is safe.

The shared crate has reference-vector, property, malformed-message and seeded fuzz
coverage. Native channel and storage tests cover real commitment rounds, delayed
monitor completion, parking, abort, reload and compatibility with older readers.
Local LND tests exercise actual commitment evidence and atomic database guards.
These tests do not establish end-to-end offline interoperability. Regtest process
crashes, link reconnection, chain enforcement and physical mobile device lifecycle
tests remain necessary before enabling the feature.

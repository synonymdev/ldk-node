# Experimental offline receiving

The integration target is Bitkit's current rust-lightning 0.2.5 baseline and
Blocktank's LND v0.21.3-beta baseline. Work is tracked in
[issue 117](https://github.com/synonymdev/ldk-node/issues/117).

The shared protocol implementation lives in `lightning-ffor` in the
[Rust channel-engine draft](https://github.com/synonymdev/rust-lightning/pull/4).
Node pins the channel engine, companion crates and shared protocol to one exact
commit through direct dependencies in Cargo.toml and Cargo.lock. These pins also
apply when another Rust project consumes Node. Wire parsers, authenticated
setup derivation, transcript hashes, reference fixtures and fuzz tests belong to
that crate. Keeping one implementation lets Node and the channel engine use the
same validation rules.

The wallet signer forwards FFOR signing requests to its existing node identity.
The shared protocol validates the resulting single-SHA256 signature domain. No
private keys are exported, and a valid signature alone grants no channel authority.

This dependency does not enable offline receiving. The current native bindings
and ordinary invoice behavior remain unchanged. The mobile draft providers expose
no production capability until authenticated activation, durable channel ownership
and recovery are implemented together.

## Private receive transport

Node's custom-message composition retains the existing LSPS reader, outbox, features
and peer callbacks. An optional private FFOR receiver can parse the seven supported
signed lifecycle types and witness acknowledgement type 55057 through the shared
canonical codecs. The production builder leaves this receiver disabled. This slice
has no FFOR sender, feature advertisement, protocol transition or public setting.

The transport accepts peer identity only from PeerManager's authenticated callback.
Each successful connection gets a distinct opaque token. Disconnect or replacement
clears its queued work, and a failed LSPS connection callback cannot establish a new
FFOR connection. Popped inputs retain the original token and exact wire bytes. A
consumer must recheck connection ownership under the actual native channel authority
before applying a transition; a successful parse or point-in-time token check grants
no such authority. Claimed wire identities cannot replace the authenticated peer.

Frames are limited to 65,535 bytes including the message type. The receive queue
tracks at most 64 peers, eight frames and 256 KiB per peer, and 128 frames and 1 MiB
globally. Those byte limits cover retained wire payloads; bounded peer and frame
metadata adds fixed overhead. Queue refusals preserve existing work and do not
disconnect ordinary peers. Consumers take one frame at a time and remain responsible
for bounding any work they retain after removal from the queue. Debug formatting
exposes only the FFOR type and length, so PeerManager trace logs cannot print
lifecycle preimages or other wire payloads. Malformed recognized frames fail the
custom reader; unknown types keep the
existing ignore behavior. Witness service requests and receipt retrieval are outside
this receiver slice.

Run `cargo test --lib message_handler` for exact public fixture routing, LSPS
coexistence, disabled behavior, malformed frame bounds, connection replacement,
concurrent disconnect and property-based queue accounting checks. Fixture provenance
is recorded in `src/message_handler/test_data.json`. Parser fuzzing remains in the
shared `lightning-ffor` wire and witness targets; Node also runs arbitrary-byte
property checks at the length-limited reader boundary.

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

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

## Private bounded transport

Node's custom-message composition retains the existing LSPS reader, outbox, features
and peer callbacks. An optional private FFOR receiver can parse the seven supported
signed lifecycle types, witness acknowledgement type 55057 and fetch response type
55061 through the shared canonical codecs. Its bounded outbox accepts exact wire messages from a future
native-authorized release callback. The production builder leaves this transport
disabled. This slice has no operational sender, feature advertisement, protocol
transition or public setting.

The transport accepts peer identity only from PeerManager's authenticated callback.
Each successful connection gets a distinct opaque token. Disconnect or replacement
clears its queued work, and a failed LSPS connection callback cannot establish a new
FFOR connection. Popped inputs retain the original token and exact wire bytes. A
consumer must recheck connection ownership under the actual native channel authority
before applying a transition; a successful parse or point-in-time token check grants
no such authority. Claimed wire identities cannot replace the authenticated peer.

Frames are limited to 65,535 bytes including the message type. The combined queues
track at most 64 peers, eight frames and 256 KiB per peer, and 128 frames and 1 MiB
globally. Those byte limits cover retained wire payloads; bounded peer and frame
metadata adds fixed overhead. Queue refusals preserve existing work and do not
disconnect ordinary peers. Consumers take one frame at a time and remain responsible
for bounding any work they retain after removal from the queue. Debug formatting
exposes only the FFOR type and length, so PeerManager trace logs cannot print
lifecycle preimages or other wire payloads. Malformed recognized frames fail the
custom reader; unknown types keep the existing ignore behavior. Inbound witness
service requests remain unhandled. A parsed fetch response is uncorrelated encrypted
input until the recovery owner verifies the pending request, connection and manifest.

Outbound witness provisioning (55055) and signed fetch requests (55059) accept only
the shared immutable `Provision` and `SignedFetch` types. Their constructors require
an authenticated setup or the trusted mailbox fetch key. Raw request bytes cannot
bypass those checks, and the Noise peer key is never substituted for the fetch key.
Provisioning still requires durable secrets and current native authority. Fetching
historical evidence does not establish current activation or payment authority.

An outbound enqueue checks the original connection token and shared capacity in one
critical section. Backpressure preserves all queued messages, and an exact retry
already in the outbox consumes no additional capacity. Queue acceptance is not
network delivery. Disconnect clears both queues, and old tokens cannot enqueue on a
replacement connection. The custom-message drain preserves each producer's FIFO
ordering alongside LSPS. The pinned PeerManager holds its peer-map read lock through
draining, selecting the same peer and encrypting into its socket queue; disconnect
and replacement require the write lock. Native code remains responsible for the
persistence and phase checks before enqueueing and for deciding whether any later
replay is legal. No transport mutex may span a native manager call.

Run `cargo test --lib message_handler` for exact public fixture routing, LSPS
coexistence, disabled behavior, malformed frame bounds, connection replacement,
concurrent disconnect, outbound backpressure, typed witness requests, bounded fetch
responses and property-based queue accounting checks. Fixture provenance is recorded
in `src/message_handler/test_data.json` and beside the witness key-operation fixtures.
Parser fuzzing remains in the
shared `lightning-ffor` wire and witness targets; Node also runs arbitrary-byte
property checks at the length-limited reader boundary.

## Private witness secret storage

The private `ffor::witness_store` module retains an immutable encryption key per
epoch, plus a separate fetch key, mailbox and exact signed manifest per witness.
It has no builder, transport or payment-provider caller. Its non-test binding
constructor accepts only an opaque native recovery context with a retained activation
acknowledgement. It binds the native context digest, original funding output,
identities and exact signed setup and activation. Historical evidence and successful
secret storage do not establish current activation or invoice readiness. Runtime use
still requires a current native authority check.

The store derives a dedicated wrapping key from the wallet seed using HKDF with
HMAC-SHA256. It uses the existing pinned VSS client's ChaCha20Poly1305 primitive
with a fresh operating-system nonce, even for local storage. Associated data binds
the storage key, schema and exact epoch evidence. Load authenticates the envelope,
rechecks the manifests and compares their public keys with the retained secrets.
Own secret buffers are zeroized and debug output is redacted. The dependency makes
additional plaintext allocations which it does not zeroize; complete memory erasure
is not claimed. The construction follows [HKDF](https://www.rfc-editor.org/rfc/rfc5869)
and [ChaCha20Poly1305](https://www.rfc-editor.org/rfc/rfc8439).

One exclusive owner serializes the namespace. Admission is bounded to four witnesses
per epoch, 512 KiB per encrypted record, 64 epochs and 8 MiB overall. Records are not
evicted or replaced to admit new work. A failed write blocks that owner until the
exact retained ciphertext is written successfully. Readable bytes alone do not prove
durability: a filesystem rename can become visible before directory synchronization
succeeds. Reopening likewise requires an authenticated, byte-identical successful
rewrite before returning restored material. No retry creates replacement keys or
manifests for an existing record.

The threat boundary includes untrusted peers, malformed or altered storage, failed
entropy, interrupted writes and concurrent callers within one owner. Store success
is the backend's durability contract. Authenticated encryption does not detect a
rollback to an older valid backup or protect against wallet-seed compromise. KVStore
has no cross-process compare-and-swap, so concurrent independent owners are not
supported. A future native registration must distinguish a new epoch from an
existing epoch whose entire sidecar is missing; `load` never regenerates secrets.
Protected key operations sign fetch requests and decrypt witness records without
exporting either private key. Each fetch call draws a fresh operating-system request
identity, 256-bit nonce and signature entropy, including retries after a lost response
or restart. The witness still enforces replay refusal. Decryption authenticates the
retained witness and manifest before native AEAD and body checks. These helpers do not
correlate a response connection, persist a receipt, change a channel or credit a
payment. Witness acknowledgement persistence and runtime recovery remain separate work.

Run `cargo test --lib ffor_witness` for exact retries and restart, failed writes
with readable bytes, corruption, wrong seeds and bindings, key separation, capacity,
concurrent creation, fallible entropy, truncated records and property-based mutation
checks, fresh fetch authorization after restart and protected decryption of pinned
Beignet records. These checks do not establish complete branch coverage or process-crash
interoperability.

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

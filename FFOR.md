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
disabled. This slice has no feature advertisement or public setting.

The private receiver adapter uses the concrete native manager and pairs its opaque
authenticated generation with the transport token in the same bounded peer map.
Accept, ActivateAck, Abort and CloseAck are processed synchronously before the custom
callback returns, so following ordinary HTLC frames cannot overtake native ownership.
A native error requests disconnection and never falls back to the mailbox. Other
unsupported lifecycle inputs and witness replies remain bounded queued work. Failed
LSPS connection callbacks, disconnect and replacement clear both tokens and queues.
Preparation, exact wire release, close intent, retry lookup and cancellation delegate
to native authority. Advancement first asks the native planner for its next operation.
Only a native request for proof reads the concrete ChainMonitor; the adapter drops
that monitor guard before passing the opaque snapshot and original paired generation
back to the manager. Stock peer events, STFU, commitment rounds and persistence must
continue between advances. The adapter retains no independent lifecycle state and
does not treat Active progress as invoice readiness. No builder constructs it yet;
runtime scheduling and payment recovery remain necessary before
offering offline invoices.

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

## Private witness recovery storage

The private `ffor::witness_store` module retains an immutable encryption key per
epoch, plus a separate fetch key, mailbox and exact signed manifest per witness.
A private witness owner composes its storage and transport operations; no production
builder or payment provider constructs either owner. Its non-test binding
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

One exclusive owner serializes the secret and receipt namespaces. Secret admission
is bounded to four witnesses per epoch, 512 KiB per encrypted record, 64 epochs and
8 MiB overall. The separate receipt namespace permits at most 1 MiB per epoch and
8 MiB overall. Records are not evicted to admit new work. Keys and signed manifests
cannot be replaced. A failed write blocks that owner until the
exact retained ciphertext is written successfully. Readable bytes alone do not prove
durability: a filesystem rename can become visible before directory synchronization
succeeds. Reopening likewise requires an authenticated, byte-identical successful
rewrite before returning restored material. No retry creates replacement keys or
manifests for an existing record.

The store also retains each selected witness's first exactly correlated provisioning
acknowledgement. It rechecks the complete immutable manifest and retention promise
before sealing the update. Subsequent requests cannot replace the first promise.
Fixed acknowledgement slots are reserved when keys are created, so recording all
promises does not grow the record. Historical schema 1 records load without promises;
their first acknowledgement update upgrades the format only within capacity.

New schema 3 records also reserve encrypted evidence capacity before any material is
returned. The fixed receipt book contains one slot per selected witness and native
voucher. Creation first writes the immutable secrets with a pending allocation, then
the empty encrypted receipt book, then the same secrets marked reserved. Interrupted
creation resumes with exactly those keys and manifests. Inventory accounting charges
the complete reservation even when the pending receipt write has not appeared yet.
A missing or damaged completed reservation is never recreated. Existing schema 1
and 2 records remain available for historical key operations and acknowledgements,
but cannot be provisioned or silently acquire a new receipt reservation.

Each received record must match the selected witness and immutable manifest and pass
native authenticated decryption and voucher checks before retention. The receipt book
stores the signed encrypted core, excludes unsigned guardian attachments and contains
no plaintext preimages. Its wrapping key and associated-data domain are separate from
secret storage. Identical cores deduplicate; a different valid core for the same
witness and slot is reported as a conflict while preserving the first evidence.
Loading retained evidence confirms durability and authenticates it again. An empty
slot or completed fetch cannot establish that a voucher is unpaid.

A failed update retains the exact proposed ciphertext and its original predecessor.
Retry accepts only those bytes and always requires another successful write. This
rule applies across both namespaces. A deleted existing record or conflicting
replacement blocks recovery. Restored promises likewise
require successful durability confirmation before being returned. Historical promises
from every selected witness are necessary storage evidence, not current native
provisioning authority or invoice readiness.

The threat boundary includes untrusted peers, malformed or altered storage, failed
entropy, interrupted writes and concurrent callers within one owner. Store success
is the backend's durability contract. Authenticated encryption does not detect a
rollback to an older valid backup or protect against wallet-seed compromise. KVStore
has no cross-process compare-and-swap, so concurrent independent owners are not
supported. Native immutable registration distinguishes a new epoch from an
existing epoch whose sidecar is missing; the witness owner uses load-only recovery
for registered epochs, and `load` never regenerates secrets.
Protected key operations sign fetch requests and decrypt witness records without
exporting either private key. Each fetch call draws a fresh operating-system request
identity, 256-bit nonce and signature entropy, including retries after a lost response
or restart. The witness still enforces replay refusal. Decryption authenticates the
retained witness and manifest before native AEAD and body checks. These helpers do not
correlate a response connection, change a channel or credit a payment. Receipt
retention preserves authenticated evidence for native reconciliation. Witness
transport and the private original-monitor recovery caller are described below.

Run `cargo test --lib ffor_witness` for exact retries and restart, failed writes
with readable bytes, corruption, wrong seeds and bindings, key separation, capacity,
concurrent creation, fallible entropy, truncated records and property-based mutation
checks, fresh fetch authorization after restart, protected decryption of pinned
Beignet records, exact acknowledgement retention, concurrent acknowledgements, failed
updates and sealed legacy-record upgrades. Receipt tests cover all initialization
write boundaries, visible failed writes after restart, exact deduplication, witness
equivocation, allocation quotas, missing reservations, legacy refusal and concurrent
acknowledgement and receipt retention. Fixture provenance is recorded beside the
receipt tests. These checks do not establish complete branch coverage or process-crash
interoperability.

## Private witness owner

The concrete `ffor::witness_owner` composes the actual ChannelManager, ChainMonitor,
protected store and authenticated transport. Exclusive access serializes bounded transient
work, with a combined 64-request and 8 MiB encoded-payload reservation across
provisioning and fetches. Each fetch reserves its complete response, request and
bounded pagination history before release. Fixed metadata has a separate count
bound. Capacity refusal preserves admitted work.

New registration first completes all protected sidecar writes, then registers the
exact immutable selection in native state. Existing native registration requires
load-only recovery with identical manifests, selected identities and key metadata.
Provisioning waits for the native registration barrier and fresh Active authority.
Native release rechecks phase, deadline, original commitment evidence and current
persistence under its transition locks; its callback only checks the witness's
original transport token and enqueues. No storage or network operation holds those
native locks. The settlement peer need not be connected for witness provisioning.

The owner stages exact response correlation in both Node and native state before queue
acceptance. A sealed pair binds the native witness generation to the actual transport
token. Native marks an attempt sent only after the typed enqueue succeeds. Backpressure
keeps the same unsent request; accepted requests are not resent after transport drain.
Explicit timeout retry uses a fresh request ID with the same manifest. A candidate
correlation table preserves the original pending request if native staging refuses,
including when its independent 64-attempt/tombstone budget is full.

Acknowledgements must match the actual witness, original connection and exact sent
request. The owner rejoins native history and protected manifests, confirms its sidecar
write, then retains the promise natively on that same authenticated generation. Pending
native writes keep correlation intact. A correctly correlated refusal retires the attempt
without creating a promise. Disconnect between the two successful writes retains the
first sidecar promise and requires a fresh native attempt after reconnect.

Each layer preserves its own first valid promise. After a crash, their request IDs and
adequate retention values may differ. Recovery joins their exact epoch, manifest and
witness, confirms protected storage and waits for the latest native persistence barrier.
It never overwrites either promise to match transport IDs. The reported progress concerns
persistence only; it grants no invoice or payment authority. Restored native records need
a fresh successful manager write even when both historical promises are present.

Historical fetches remain available after channel removal and admission expiry.
Each traversal and retry uses fresh signed request identity and nonce. Every page
must match the actual witness and connection, retained manifest, request, signed
records and strictly increasing cursor. Each encrypted record becomes durable
before the cursor advances. Checked pages survive disconnect and uncertain writes;
recovery must complete the exact write before they can be replaced. Once retained,
evidence remains even if a later page is invalid or the witness disappears.

Candidate-only authentication failures and valid conflicting cores are distinct
from local storage corruption. The owner rejects those candidates while retaining
later valid evidence from the same page. Once all such evidence is durable, it
reports a rejected page and permits a fresh traversal from slot zero. Storage
uncertainty or local corruption keeps the page and its cursor intact. A completed
or empty traversal does not establish that any slot is unpaid. Fetching emits no
payment credit or native claim. Each authenticated page retains all valid
cores with one fixed-book write, and an exact retry performs no additional write.
This avoids revalidating and rewriting the complete book for every candidate.
Large-book recovery performance still needs measurement before production scheduling.

Tests restore a genuinely funded and signed native Active manager and stock monitor
through NodeBuilder with a public test seed. They cover registration persistence,
all three sidecar write boundaries, failed and stale manager persistence tokens,
exact queued retries, fresh timeout identities, current witness correlation,
missing-sidecar refusal after reload, historical acknowledgements after channel
removal, shared quotas, fetch backpressure, pagination, disconnect and storage
failures, rejected candidates followed by valid evidence, one write per page,
exact batch recovery and zero payment credit.
Fixture provenance is in `src/ffor/witness_owner/fixtures/README.md`; test-only
witness encryption uses ring against the retained public epoch key and never
exports a protected private key. No builder or scheduler enables this owner yet.

The separate receipt-recovery call first rejoins immutable native witness
registration and confirmed protected evidence. It captures an opaque snapshot from
the actual ChainMonitor, drops that monitor guard and asks the native manager to
import the authenticated preimage. Native rechecks the original funding output,
current monitor counter and exact retained voucher under its channel locks.
Historical recovery remains possible after deadline, disconnection and channel
removal. An absent receipt means only that this store has no evidence for that slot.
Missing or uncertain sidecar storage cannot reach the monitor.

A submitted monitor update is reported as pending, even if Node's synchronous
MonitorUpdatingPersister has already completed its write. Normal monitor-event
processing and a fresh retry observe completion. Neither observation credits a
payment or authorizes an invoice. Tests use the actual Node persister and restore
its durable monitor updates; they never manufacture a monitor-completion signal.
They cover live and archive-only recovery, idempotence after restart, and uncertain
receipt writes before native import. Delayed and failed native monitor writes are
covered in the pinned channel engine. Authoritative settlement outcomes and the
production recovery scheduler remain unfinished.

## Private durable receive requests

The private `ffor::request_store` retains the original client request ID, fixed
positive amount, exact description bytes, selected channel and settlement peer,
and every native preparation parameter before allocating an epoch. It captures
the actual Node network, identity and manager. Local request IDs derive from the
chain, node identity and length-framed client ID; amount and description are not
part of that lookup key, so changed retry arguments are refused.

Requests use a separate wallet-seed-derived wrapping key and storage namespace.
Authenticated envelopes bind the actual node, chain, schema and storage key. One
exclusive owner serializes admission and native binding. A failed or reopened
write requires a successful byte-identical confirmation before use, and uncertain
recovery accepts only its exact ciphertext or retained predecessor. Deletion or
replacement cannot reconstruct missing intent from a native selector. As with the
witness store, valid-backup rollback and independent concurrent owners are outside
the storage contract.

Lookup precedes fresh liquidity selection. A native request without its application
record refuses recovery rather than inventing a description or allocating again.
Preparation uses the exact stored native parameters and a genuine retained-peer
connection, then rejoins the same application record before persisting the returned
opaque selector. The separate historical recovery call checks every stored parameter
against native history without requiring a connection or mutating the native manager.
It can complete a lost selector-binding write using the same protected record and
exact-write recovery rules. Selector lookup alone does not certify intent or
readiness. A bound record with missing native history refuses replacement; absence
for an unbound intent does not itself authorize new allocation. Detaching a caller
leaves its durable intent recoverable and does not imply cancellation.

Legacy version 1 records reserve 4 KiB per request, including native binding, and
remain readable. An explicit version 2 upgrade reserves 8 KiB before any native
invoice issuance, for at most 64 records and 512 KiB total. Unbound intents count
toward that limit; there is no eviction. Version 2 retains the fixed invoice policy,
the exact signed invoice, its native context digest, a fixed payment timestamp and a
monotonic confirmation digest of the exact Pending payment row. These are historical
storage facts, never a readiness flag, cancellation outcome or completion ledger.
Native epoch reuse and historical retention policy are still required for repeated
receives on one channel.

The issuer adapter joins one request to native invoice assignment in a fixed order:
durable policy reservation, native preparation, native persistence, exact invoice
retention in the protected record, exact inbound Pending payment confirmation through
a successful payment-store write, and the protected confirmation marker. Description,
amount and route terms come from the record and native ownership; the caller supplies
only a policy and public witness-to-settlement route evidence. A different policy is a
conflict, the reserved policy is never replaced, and an existing native assignment is
recovered before any new preparation, so fresh route evidence is accepted on retry.
Every pending native manager or monitor write reports awaiting persistence instead of
progress. No production completer for native persistence tokens exists yet.

The payment store confirms the exact Pending Bolt11 row against memory and disk and
persists before installing it in memory. A payment confirmation write that fails
ambiguously keeps the exact expected candidate in memory and blocks every owner
operation, including previously minted handles, until the same confirmation succeeds.
Restart drops the candidate; the missing marker then forces the same idempotent
confirmation before a handle exists. A definite refusal (missing, corrupt, conflicting
or terminal payment) clears the candidate and fails closed without touching native.

A publication handle is opaque, bound to the owner instance and the manager instance,
and exposes no invoice bytes. Minting it re-reads the protected record, rechecks the
exact native bytes, digest, intent and epoch context, and performs another successful
payment write. Release re-reads the record from storage, revalidates the handle,
acquires the payment-store exclusion and only then enters the native monitor guard,
whose innermost callback moves the exact bytes into the caller's slot and performs no
I/O. Repeated release is allowed. Native refusal after expiry, deadline or a moved
monitor tip leaves the record, native assignment and Pending row unchanged. Generic
payment removal now holds its object lock through the disk removal so a deletion
cannot race past a new confirmation.

Tests restore genuine empty and pending native channel fixtures through NodeBuilder.
They cover exact retries, native intent mismatches, wrong or stale connections,
missing application or native history, visible failed writes and restart, exact
binding recovery, disconnected exact-history recovery without native mutation,
identity substitution, corruption, replacement and quota refusal.
The bounded record codec also has arbitrary-byte property checks, strict version 2
framing, monotonic markers and full-capacity envelopes. Issuer tests restore the genuine
public-driver invoice fixture (`src/ffor/request_store/fixtures/invoice/README.md`)
and re-sign fresh route evidence with its public witness seed. They cover live issuance
through native persistence, exact byte and payment retention, repeated release with no
extra writes, failure at every application write with the exact candidate retained and
all handles blocked, restart with a fresh native barrier and obsolete handles refused,
historical recovery of an expired assignment that never publishes, conflicting policy
and stale route refusal without a native slot, missing, corrupt and terminal payment
rows, unbound records, missing native history, a monitor tip ahead of the manager, and
zero storage I/O under publication. Run `cargo test --lib ffor_request` for the
request-store, codec and issuer checks and `cargo test --lib ffor_payment` for the
payment-store checks. No production caller constructs this owner yet.

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

# FFOR protocol foundation

This unpublished crate implements checked Variant D amount and anchor-channel book
calculations, signed setup and lifecycle codecs, canonical books and authenticated
transcript checks from draft v0.9.4. It is not connected to `Node`, the payment
handlers, custom peer messages or UniFFI.
It cannot prepare an offline invoice or receive a payment.

Work is tracked in [ldk-node #117](https://github.com/synonymdev/ldk-node/issues/117).

## Channel-engine baseline

The receiver port targets rust-lightning **v0.2.5**, pinned to
[`5bc1dc84b3a1b084f84de4b7ece3d978b678d894`](https://github.com/lightningdevkit/rust-lightning/commit/5bc1dc84b3a1b084f84de4b7ece3d978b678d894).
This retains the current LDK Node dependency generation and its maintenance fixes.
The Synonym fork's `main` snapshot uses `0.3.0+git` and requires a separate API
migration. It is not the baseline for this port.

## Reference and verification scope

The normative source is FFOR at
[`d719161f42d1eeb6bd6c3856d564222f03c2205e`](https://github.com/coreyphillips/ffor/tree/d719161f42d1eeb6bd6c3856d564222f03c2205e).
`tests/data/appendix-d.json` contains the public Appendix D transcript fixtures.
The extraction script reconstructs the abbreviated D.5 messages and book using the
published deterministic inputs and existing signatures, then checks the published
wire hashes and book hash before writing the fixture. No signing keys are needed.

```sh
python3 crates/ffor-protocol/tests/data/extract_appendix_d.py /path/to/ffor/ffor-variant-d-vectors.md
cargo test -p ffor-protocol --locked
cargo clippy -p ffor-protocol --all-targets --locked -- -D warnings
cargo fmt -p ffor-protocol -- --check
```

Tests cover the six published transcript scenarios, including the 483-slot book,
verification of their public node signatures, both funder roles, both commitment
dust limits, an unfunded receiver, exact budgets, fee-spike reserves, negotiated
limits, deadlines, overflow and public-channel fee selection. Property tests compare
arithmetic with a wide-integer oracle and mutate budgets, amounts and transcript
domains. They do not construct or broadcast commitment transactions.

The wire parser supports `ff_init`, `ff_accept`, `ff_activate`, `ff_activate_ack`,
`ff_abort`, `ff_close` and `ff_close_ack`. It bounds message sizes and collection
counts, rejects noncanonical BigSize values, unknown mandatory TLVs and invalid
compressed points, and preserves unknown optional fields in the signed bytes.
Low-S compact signatures are verified against expected channel peer identities.
Unsigned digest construction does not require a placeholder valid signature.

`AuthenticatedSetup` derives the book exclusively from authenticated setup messages.
It checks fee bounds, unique hashes, requested hash chains, activation transcripts,
height agreement and close preimages, including prefix settlement for chained books.
It is immutable protocol data, not an epoch state machine or proof of live capacity.

`tests/data/beignet-lifecycle.json` adds five signed reference lifecycle fixtures,
including both absent and explicitly empty preimage TLVs when no payment settled.
Regenerate them with `generate_beignet_lifecycle.cjs` against the pinned Beignet
checkout, using that checkout's `ts-node/register`. Both encodings are preserved
exactly so authentication never depends on normalizing received signed bytes.

The standalone [fuzz target](fuzz/README.md) exercises parsing, canonical round trips,
signatures and authenticated setup with real signed fixture seeds. A bounded local
run completed 6,359,059 inputs in 121 seconds without a crash. The current suite has
61 tests and three doctests. Nightly LLVM instrumentation measured 838/859 source
lines (97.56%) and 196/198 branches (98.99%) covered across this crate. Uncovered
code includes diagnostic formatting and defensive paths whose preconditions are
excluded by prior validated bounds. These are measured results, not exhaustive
proofs of correctness.

There is no signer, persistence implementation or channel state machine in this
crate. Crash injection, monitor recovery and cross-engine regtest remain requirements
for the engine port. Passing pure protocol tests is not evidence of offline payment
settlement or recovery.

## Implementation references

The receiver and settlement port should be compared with these immutable source
revisions, in addition to the normative specification:

- [Beignet 0.21.10, `8aee31d18e596fe49a0d195b325a6e757d7a009b`](https://github.com/coreyphillips/beignet/tree/8aee31d18e596fe49a0d195b325a6e757d7a009b).
  `src/lightning/channel/channel.ts` owns voucher matching, both-view commitment
  verification, activation, freeze and drain. `src/lightning/ffor/` contains the
  wire, transcript and witness code. The Variant D setup and settlement tests
  cover acknowledgement loss, restart, rejected updates and cooperative return.
- [beignet-umbrel, `12d483462ca2eabd8ae9cc105e69affa420459f9`](https://github.com/coreyphillips/beignet-umbrel/tree/12d483462ca2eabd8ae9cc105e69affa420459f9).
  `manager/ui/src/pages/tabs/ReceiveTab.jsx` and the receive routes demonstrate
  explicit offline intent, stable request identity, existing-channel capacity
  and rejecting a response that is not explicitly offline-capable. The
  `scripts/lfbw-regtest/` FFOR scenarios exercise process-stopped receiving and
  return through the manager and daemon APIs.

These sources implement their own channel engine. They do not supply FFOR APIs
to rust-lightning or LND. Port the protocol invariants into each engine's own
commitment and persistence boundary. Do not copy deployment defaults such as
channel headroom or invoice lifetime into Bitkit as protocol guarantees. Passing
reference tests alone cannot qualify the native port.

## Trust boundaries

- Amounts, peer data and public policy are untrusted. Overflow, underpayment,
  overpayment and inconsistent budgets must fail before committing vouchers.
- Only the channel engine may supply channel type, negotiated limits, balances,
  funder identity and dust limits. Application aggregate inbound liquidity is not
  sufficient. Existing ordinary HTLCs must be drained before book validation.
- A public fee exception requires an authenticated local public channel to the
  correct receiver, all four announcement signatures, correct node/funding-key
  bindings and the onion's actual SCID. This crate only compares fee amounts.
- Hash functions operate on public transcript bytes and authenticate nothing on
  their own. Callers must validate canonical encodings and low-S signatures. The
  existing signer must retain all private keys.
- A valid book is a proposed reservation, not received money. Neither amount checks
  nor matching transcript hashes imply durable activation or invoice readiness.

There is no new unsafe code, secret storage, nonce generation or custom crypto.
SHA256 and signature verification use the existing Bitcoin dependency and its
secp256k1 implementation. No production signing API is introduced.

## Required integration

The registry `lightning 0.2.5` used by ldk-node has no FFOR receiver support.
[rust-lightning #4](https://github.com/synonymdev/rust-lightning/pull/4) adds a
point-in-time verifier for actual committed vouchers and monitor claim signatures
on that baseline. It is not wired into this crate. The next engine changes must
own voucher recognition, both-view commitment completion,
persistent ACTIVE freeze, activation acknowledgement replay, cooperative drain and
on-chain enforcement inside rust-lightning. A second channel state machine in
ldk-node would duplicate signing authority and is not an acceptable substitute.

After that integration, ldk-node must provide exact-amount eligibility, durable and
retry-safe preparation, invoice exposure, recovery and status through generated
Kotlin/Swift bindings. Readiness must require the agreed witness acknowledgements.
The Bitkit provider adapters must only be enabled after these real APIs exist and
the interoperability tests demonstrate payer success with the receiver stopped.

The first app profile is a positive fixed amount in whole satoshis, single-part
BOLT 11, one eligible anchor channel, with a `Receive Offline` checkbox. Amountless
invoices, just-in-time liquidity and aggregate multi-channel capacity cannot qualify.
The node must enforce the selected offline window, claim margin, chain watching and
fee funding. No production window or witness deployment is chosen by this crate.

The requested settlement baseline is LND v0.21.3-beta. LND implementation work is
kept local. No native dependency, binding version or release is changed here.

# FFOR offline-receive regtest harness

End-to-end run of the opt-in offline-receive runtime (`FFOR.md`, "Opt-in receiver
runtime") with LDK Node as the receiver R and Beignet reference daemons as the
settlement peer S, the witness W and the payer X. The daemons are managed by the
beignet-umbrel manager; the receiver is the `ffor_regtest_receiver` cargo example.

## Prerequisites

- A Polar-style regtest stack: `bitcoind` in a docker container named `bitcoin`
  (RPC 43782, `polaruser`/`polarpass`, wallet `default`) and an electrs at
  `127.0.0.1:60001`. The chain helpers shell out to
  `docker exec bitcoin bitcoin-cli ...` for funding and mining.
- Beignet built: `../beignet/dist/cli/cli.js` (override with `BEIGNET_BIN`).
- beignet-umbrel manager dependencies installed in `../beignet-umbrel/manager`
  (override the checkout with `BEIGNET_UMBREL_DIR`). The script imports
  `scripts/lfbw-regtest/lib.mjs` from that checkout for the manager, daemon and
  chain helpers.
- Node.js 22 and a Rust toolchain.
- Free TCP ports 3900 (manager), 3901-3950 (daemons; their Lightning listeners
  are the daemon port plus 6000) and 9739 (receiver, `RECEIVER_PORT`).

## Run

```sh
cd ldk-node
BITCOIND_SKIP_DOWNLOAD=1 ELECTRSD_SKIP_DOWNLOAD=1 \
  cargo build --example ffor_regtest_receiver --locked --offline
node scripts/ffor-regtest/e2e.mjs 2>&1 | tee /private/tmp/ffor-e2e-run.log
```

The script starts the manager itself (`PORT=3900`, `DATA_DIR=/private/tmp/ffor-e2e/manager`),
creates and funds S, W and X, opens W -> S (1,000,000 sats) and X -> W
(500,000 sats), starts the receiver, funds it with 1,000,000 sats and lets it open
a 500,000 sat anchor channel to S pushing 400,000 sats to S (Beignet's v1 open pins
the anchor commitment feerate to 253 sat/kw, which LDK refuses on a regtest whose
bitcoind carries real fee estimates, and LDK does not offer dual funding), and then
pushes S's channel policy for that channel so S sends its private-channel
`channel_update` (Beignet sends none on its own; LDK's native invoice binding needs
S's forwarding terms), and then drives the round:

1. R prepares one 50,000 sat offline receive (`e2e-1`) and prints `INVOICE`.
2. R is killed with SIGKILL. A restart before payment must report the identical
   invoice (negative case), then R is killed again.
3. X pays through W while R is down; W must hold exactly one record and S must
   mark slot 1 settled.
4. R restarts, re-releases the invoice, and the script mines to the deadline
   safety margin so the runtime closes the epoch cooperatively. Expect exactly
   one `PaymentReceived` event, `Settled { Fulfilled }` and a `Succeeded`
   payment row.
5. A final restart must report `Settled` again with no second event.

Every check prints a `PASS`/`FAIL` line and the run ends with `RESULT PASS` or
`RESULT FAIL (n)`. All harness state lives under `/private/tmp/ffor-e2e`
(`FFOR_E2E_DIR`) and is removed at the start of each run. Logs:
`/private/tmp/ffor-e2e-manager.log`, `/private/tmp/ffor-e2e-receiver.log` (the
receiver's line protocol) and `/private/tmp/ffor-e2e/receiver/ldk_node.log` (the
receiver's LDK log at trace level). Daemon logs are readable through the manager
while it runs (`GET /api/wallets/:id/logs`). The script stops the receiver, every
daemon and the manager before exiting.

## Receiver line protocol

`ffor_regtest_receiver <storage_dir> <listen_port> <electrum_url> <s_node_id>
<s_host:port> <w_node_id> <w_host:port> <info|serve|status> [request_id] [amount_msat]`

- `info` prints `NODEID <pubkey>` and exits.
- `serve` starts the node, connects to S and W, prints a funding `ADDRESS`, waits
  for funds (`FUNDED`), opens the channel to S (`OPENED`), waits for it to be ready
  (`CHANNEL_READY`), prints `AWAITING_PREPARE`, prepares the request once a
  `prepare` line arrives on stdin, and follows it.
- `status` restarts from the same storage directory and follows the request.

While following, the process prints `STATUS <OfflineReceiveStatus>` on every
change, `INVOICE <bolt11>` when `Ready`, `PAYMENT ...` rows at start and stop,
and `EVENT ...` for every node event. It stops cleanly when stdin closes.

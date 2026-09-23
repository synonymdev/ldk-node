These are public deterministic test fixtures, not wallet backups.

The native `ffor_invoice_public_driver_one_slot_fixture_preserves_native_request` test generated
them through the real public receiver driver in a funded Testnet channel using wallet seed
`[91; 64]`, matching `NodeBuilder`, with a third node acting as the registered witness. The exporter
is published in native revision `a00512b79f33e2db9b1bea691ab3637122c41564`, whose source base was
`016778d`. Its output directory is selected only with `FFOR_NODE_INVOICE_FIXTURE_DIR`; ordinary
tests write no fixture files.

`active-manager.bin` and `monitor.bin` capture the receiver after activation, witness
registration and every durable witness acknowledgement, before any invoice exists.
`issued-manager.bin` captures the same receiver after one native invoice assignment; that historical
invoice expires one hour after export and its stored route update ages past native freshness, so
it exercises historical recovery and refusal only. `route-announcement.bin` and
`route-update.bin` are the signed public witness-to-settlement gossip used at export; live
issuance tests re-sign a fresh update with `witness_node_seed`. `witness-manifest.bin` records the
selected witness manifest for provenance. `fixture.txt` lists the exact identity, channel, setup and
intent parameters. No fixture constructs a readiness or payment capability.

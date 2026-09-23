These are public deterministic test fixtures, not wallet backups.

The native `ffor_export_node_request_fixture` test generated them in a real funded Testnet
channel using wallet seed `[91; 64]`, with the receiver signer seed derived through Bitcoin
BIP32 master private-key derivation, matching `NodeBuilder`. `fixture.txt` records the exact
identity, channel and setup parameters. The exporter is published in native revision
`0def88713b26264ab50c2451ba97a17839f331ac`, whose source base was `7ed8161b7`. Its output directory is
selected only with `FFOR_NODE_REQUEST_FIXTURE_DIR`; ordinary tests write no fixture files.

The empty pair precedes any FFOR admission. The pending pair retains an actual signed native
Init and interception gate, before any Init leaves. `request-fixture` maps to the listed
local request ID through the Node domain-separated request-ID derivation. The Node tests
restore both pairs through the actual `NodeBuilder`, and use the public native exact-intent
retry to obtain opaque selectors. No fixture constructs a readiness or payment capability.

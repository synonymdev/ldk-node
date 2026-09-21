These are public test fixtures, not wallet backups.

The funded Testnet channel and signed Active epoch were produced by the native
`ffor_witness_registration_release_requires_current_durability_and_survives_archive_only_restore`
test in `lightning/src/ln/channelmanager/ffor_activation/witness/tests.rs`, using
native checkpoint `7ed8161b7e3ac375b639de5e4d55542cf936af17` and
`FFOR_NODE_WITNESS_FIXTURE_DIR` for opt-in output. The exported snapshot precedes
witness registration. The native test runs ordinary commitment rounds, derives
the activation evidence from the actual channel and monitor, and authenticates
the settlement acknowledgement.

The receiver uses the public 64-byte seed consisting entirely of `0x5b`. Both
the native fixture and NodeBuilder derive the signing seed with the private key
of `Xpriv::new_master(Network::Testnet, wallet_seed)`. No production authority
constructor or exported private wallet material is used. The fixture metadata
records the channel, epoch, funding outpoint, and public identities.

Node tests restore these bytes through the ordinary NodeBuilder path. Restored
native authority requires a fresh successful manager persistence barrier before
registration or provisioning. Test snapshots alone grant no live authority.

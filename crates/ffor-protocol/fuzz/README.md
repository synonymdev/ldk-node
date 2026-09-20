# Wire parser and setup fuzzing

The `wire` target checks the 65,535-byte envelope bound, canonical decode/encode round trips,
signature verification using the Appendix D public keys, and authenticated setup construction.
It accepts either one raw message or `[u16 init_length][init][accept]` so valid signed setup
pairs can seed the authenticated boundary. It contains no signing operation or private keys.

From this directory, run:

```sh
python3 seed_corpus.py
cargo +nightly fuzz run wire -- -max_len=131072 -max_total_time=60
```

The seed script writes the six Appendix D scenarios' messages and length-prefixed setup
pairs from `../tests/data/appendix-d.json`, plus the Beignet lifecycle fixtures. Generated
corpora, crash artifacts, coverage data and the standalone workspace lockfile are ignored.

A bounded run detects regressions but does not prove exhaustive parser coverage. The parent
crate's property tests exercise bounded arbitrary bytes, extension lengths, and settled
bitmaps independently of this target. Live network partitions, persistence and channel
transitions are outside this pure codec and require later engine integration tests.

# Witness key-use fixtures

`fixtures.txt` preserves the `scenario`, `init`, `accept`, `manifest`, `record` and
`body` lines from the native witness fixtures at rust-lightning commit
`8478895388b656b4fa674d1ede4c48f33a9688b4`. Its exporter is
`lightning-ffor/tests/data/generate_native_witness_fixtures.py` in
[the native draft](https://github.com/synonymdev/rust-lightning/pull/4).

These are public deterministic Beignet vectors, pinned to Beignet revision
`8aee31d18e596fe49a0d195b325a6e757d7a009b`, covering scenarios D.1 and D.2.
Their fetch, witness and encryption test keys are respectively the repeated-byte
values 42, 43 and 44. They are not wallet data and must never be used operationally.
The store generates fresh production keys from operating-system entropy.

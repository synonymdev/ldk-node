# Receipt retention fixtures

These tests reuse the public deterministic D.1 and D.2 encrypted records documented
in [the key-use fixtures](../record/key_use/README.md). They contain public test keys
and preimages, never wallet material.

`activation-fixtures.txt` is an exact projection of the first two records in
`lightning-ffor/tests/data/appendix-d.json` at native revision
`a20b34f9822e7131d229a526249c917971c25d14`: `scenario` is unchanged, `activate_wire`
becomes `activate`, and `ack_wire` becomes `ack`. These signatures let the tests use
the normal historical storage binding without bypassing activation authentication.

`equivocation.txt` contains a second valid signed and encrypted D.1 record for the
same witness and voucher. The generator changes the public record ID to repeated
byte 99 and uses repeated byte 46 as its deterministic ephemeral encryption key.
It retains the original valid payment body and signs with the public witness key
43. Beignet verifies both signature and decryption before exporting it. The test
proves a later valid conflicting record cannot replace the first retained evidence.

To regenerate, use Beignet revision `8aee31d18e596fe49a0d195b325a6e757d7a009b`
with its dependencies installed. From this directory:

```sh
TS_NODE_PROJECT=/path/to/beignet/tsconfig.json \
node -r /path/to/beignet/node_modules/ts-node/register \
  generate_equivocation.cjs /path/to/beignet \
  ../record/key_use/fixtures.txt /tmp/ffor-equivocation.txt
cmp equivocation.txt /tmp/ffor-equivocation.txt
```

The generator refuses another reference revision. Production storage generates
fresh keys and nonces from operating-system entropy.

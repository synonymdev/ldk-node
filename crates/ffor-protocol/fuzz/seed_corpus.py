"""Seed protocol fuzzing with pinned public signed fixtures, without a signing key."""

import json
from pathlib import Path

crate = Path(__file__).resolve().parents[1]
corpus = crate / "fuzz" / "corpus" / "wire"
corpus.mkdir(parents=True, exist_ok=True)
fixtures = json.loads((crate / "tests/data/appendix-d.json").read_text())
for index, fixture in enumerate(fixtures):
    for field in ("init_wire", "accept_wire", "activate_wire", "ack_wire"):
        (corpus / f"appendix-{index}-{field}").write_bytes(bytes.fromhex(fixture[field]))
    init = bytes.fromhex(fixture["init_wire"])
    accept = bytes.fromhex(fixture["accept_wire"])
    (corpus / f"appendix-{index}-pair").write_bytes(len(init).to_bytes(2, "big") + init + accept)

reference = json.loads((crate / "tests/data/beignet-lifecycle.json").read_text())
for fixture in reference["fixtures"]:
    (corpus / fixture["name"]).write_bytes(bytes.fromhex(fixture["wire"]))
for fixture in reference["reestablish"]:
    (corpus / f"reestablish-{fixture['state']}").write_bytes(bytes.fromhex(fixture["value"]))

print("Seeded 35 signed reference messages/setup pairs and 7 reconnect reports")

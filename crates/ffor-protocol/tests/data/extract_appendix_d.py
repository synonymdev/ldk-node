"""Extract pinned public Appendix D fixtures, reconstructing abbreviated D.5 bytes.

Usage: python3 extract_appendix_d.py /path/to/ffor-variant-d-vectors.md
Writes appendix-d.json beside this script. Every reconstruction is checked against
the independently published hashes, and uses published signatures without signing.
"""

import hashlib
import json
import pathlib
import re
import struct
import sys


def sha(data):
    return hashlib.sha256(data).digest()


def field(section, name):
    return re.search(r"\| `" + re.escape(name) + r"` \| `([0-9a-f]+)`", section)[1]


def tlv(kind, value):
    length = len(value)
    encoded_length = bytes([length]) if length < 253 else b"\xfd" + struct.pack(">H", length)
    return bytes([kind]) + encoded_length + value


def extract(source):
    fixtures = []
    for number in range(1, 7):
        section = source.split(f"## D.{number} ")[1].split("\n## D.")[0]
        fixture = {"scenario": f"D.{number}"}
        for label, name in [
            ("init_hash", "T_init"), ("setup_hash", "T_setup"),
            ("book_hash", "H_book"), ("commitment_hash", "H_commit"),
            ("activation_hash", "H_act"),
        ]:
            fixture[label] = field(section, name)
        for label, name in [
            ("init_wire", "ff_init"), ("accept_wire", "ff_accept"),
            ("activate_wire", "ff_activate"), ("ack_wire", "ff_activate_ack"),
        ]:
            message = section.split(f"**`{name}` (type ")[1].split("**`ff_")[0]
            if number == 5 and name in ("ff_init", "ff_accept"):
                header = bytes.fromhex(fixtures[0]["init_wire"])[2:34]
                epoch = bytes.fromhex(field(section, "epoch_id"))
                amounts = struct.pack(">Q", 546000) * 483
                hashes = b"".join(
                    sha(sha(b"ffor/vector/D.5/preimage" + struct.pack(">H", k)))
                    for k in range(1, 484)
                )
                if name == "ff_init":
                    wire = struct.pack(">H", 55001) + header + epoch
                    wire += struct.pack(">BQHQIIIIQH", 4, 263718000, 483, 546000,
                                        798992, 800000, 1000, 5000, 0, 0)
                    wire += tlv(9, amounts)
                else:
                    wire = struct.pack(">H", 55003) + header + epoch + struct.pack(">Q", 42)
                    wire += tlv(1, hashes) + tlv(7, struct.pack(">Q", 0))
                    wire += tlv(9, amounts) + tlv(11, bytes.fromhex(fixture["init_hash"]))
                signature = re.search(r"signature \(final 64 bytes\) \| `([0-9a-f]+)`", message)[1]
                wire += bytes.fromhex(signature)
            else:
                wire = bytes.fromhex(re.search(r"Wire bytes:\s*```\s*([0-9a-f]+)\s*```", message)[1])
            expected = re.search(r"SHA256\(wire bytes\) \| `([0-9a-f]+)`", message)[1]
            assert sha(wire).hex() == expected, (number, name)
            fixture[label] = wire.hex()
        if number == 5:
            book = epoch + struct.pack(">BBH", 4, 1, 483)
            for k in range(1, 484):
                payment_hash = hashes[(k - 1) * 32:k * 32]
                book += struct.pack(">H", k) + payment_hash
                book += struct.pack(">QIIQ", 546000, 800000, 798992, k - 1)
        else:
            book_section = section.split(f"### D.{number}.4 ")[1].split(f"### D.{number}.5 ")[0]
            book = bytes.fromhex(re.search(r"```\s*([0-9a-f]+)\s*```", book_section)[1])
        assert sha(b"ffor/book" + book).hex() == fixture["book_hash"]
        fixture["book"] = book.hex()
        txids = re.findall(
            r"\| `n_[RS]\^act` / `txid\(C\^[RS]\)` internal byte order \| 43 / `([0-9a-f]+)`", section
        )
        assert len(txids) == 2
        fixture["receiver_txid"], fixture["settlement_txid"] = txids
        fixtures.append(fixture)
    return fixtures


if __name__ == "__main__":
    fixture_data = extract(pathlib.Path(sys.argv[1]).read_text())
    pathlib.Path(__file__).with_name("appendix-d.json").write_text(
        json.dumps(fixture_data, indent=2) + "\n"
    )

"""Synthetic observer wire golden; requires the observer's pinned blake3 package.
Run with its private Python: python -I -B tests/fixtures/keyword_repair_wire.py.
This verifies serialization only and never opens a catalog or source artifact.
"""
import json
from pathlib import Path
import unittest
import blake3


def encoded(value):
    return json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode("utf-8")


def digest(value):
    return blake3.blake3(value).hexdigest()


class WireGolden(unittest.TestCase):
    def test_receipt_state_and_quoted_stage_roster(self):
        fixture = json.loads(Path(__file__).with_suffix(".json").read_text())
        self.assertEqual(fixture["protocol"], 1)
        for name in ("receipt", "binding", "progress", "new_receipts"):
            item = fixture[name]
            raw = encoded(item["value"])
            self.assertEqual(raw, item["json"].encode("utf-8"))
            self.assertEqual(digest(raw), item["blake3"])
        self.assertEqual(list(fixture["receipt"]["value"]), [
            "source_identity", "slot", "owner", "adapter", "input_digest",
            "result", "retained_record", "proof",
        ])
        previous = digest(b"lightroom-keyword-repair-v1")
        for item, stage in zip(fixture["roster"], ('"Keywords"', '"KeywordMemberships"'), strict=True):
            expected = item["value"]
            value = [previous, stage, expected[2], digest(item["old_outcome"].encode("utf-8")),
                     fixture["receipt"]["blake3"] if item["with_receipt"] else None]
            self.assertEqual(value, expected)
            self.assertEqual(encoded(value), item["json"].encode("utf-8"))
            previous = digest(encoded(value))
            self.assertEqual(previous, item["blake3"])
        self.assertEqual(previous, fixture["binding"]["value"]["request"]["expected_roster_blake3"])
        self.assertTrue(all(len(v) == 4 for v in fixture["new_receipts"]["value"]))


if __name__ == "__main__":
    unittest.main()

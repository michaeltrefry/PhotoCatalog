"""Supplemental protocol correctness; only tiny generated databases, no scale timings."""

import copy
import json
from pathlib import Path
import tempfile
import unittest

import catalog_benchmark as bench
import production_profiles as profiles


class ProductionProfiles(unittest.TestCase):
    def prepared_source(self, root, count=1000):
        source = root / "source"
        csv = source / str(count) / "csv"
        bench.generate(csv, count)
        for engine in ["sqlite", "duckdb"]:
            filename = "catalog.sqlite3" if engine == "sqlite" else "catalog.duckdb"
            receipt = bench.load(
                engine, source / str(count) / engine / filename, csv, 64
            )
            bench.dump(source / str(count) / f"{engine}.json", {"load": receipt})
        bench.dump(
            source / "campaign.json",
            {
                "prepared_only": True,
                "complete": False,
                "script_sha256": profiles.digest(bench.__file__),
                "counts": [count],
                "repetitions": 4,
                "fresh_repetitions": 2,
            },
        )
        return source

    def test_snapshots_are_verified_independent_and_refuse_post_mixed_sources(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = self.prepared_source(root)
            output = root / "snapshot"
            manifest = profiles.snapshot(source, output)
            self.assertTrue(manifest["complete"])
            self.assertEqual(manifest["profiles_mib"], [256, 1024, 2048])
            original = source / "1000/sqlite/catalog.sqlite3"
            db = bench.connect("sqlite", original, 64)
            db.execute("UPDATE annotations SET rating=(rating+1)%6 WHERE asset_id=1")
            db.close()
            copy_db = profiles.readonly(
                "sqlite", output / "1000/sqlite/catalog.sqlite3"
            )
            self.assertEqual(
                copy_db.execute(
                    "SELECT rating FROM annotations WHERE asset_id=1"
                ).fetchone()[0],
                bench.asset_row(1)[11],
            )
            copy_db.close()
            with self.assertRaises((ValueError, AssertionError)):
                profiles.snapshot(source, root / "rejected")
            self.assertFalse((root / "rejected").exists())
            self.assertFalse(list(root.glob(".profile-snapshot-*")))
            state = json.loads((source / "campaign.json").read_text())
            state["complete"] = True
            state["prepared_only"] = False
            bench.dump(source / "campaign.json", state)
            with self.assertRaisesRegex(ValueError, "must precede"):
                profiles.snapshot(source, root / "late")

    def test_copy_rejects_wrong_count_and_missing_annotation_state(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = self.prepared_source(root)
            expected = json.loads((source / "1000/csv/manifest.json").read_text())
            original = source / "1000/sqlite/catalog.sqlite3"
            wrong = copy.deepcopy(expected)
            wrong["expected"]["count"] = 1001
            with self.assertRaises(AssertionError):
                profiles.copy_checkpointed("sqlite", original, root / "wrong.db", wrong)
            db = bench.connect("sqlite", original, 64)
            db.execute("DELETE FROM annotations WHERE asset_id=1")
            db.close()
            with self.assertRaises(AssertionError):
                profiles.copy_checkpointed(
                    "sqlite", original, root / "missing.db", expected
                )

    def test_64mib_stress_does_not_override_one_valid_production_profile(self):
        samples = bench.distribution([1, 1, 1, 1])
        result = {
            "copy": {"proof": {"count": 1000}, "sha256": "verified"},
            "plans": {name: [[0, "plan"]] for name in bench.QUERY_SQL},
            "warm": {
                "peak_rss_bytes": 1024,
                "workloads": {
                    name: {"distribution": samples} for name in bench.QUERY_SQL
                },
            },
            "fresh": {
                name: {"errors": [], "open_plus_query": samples, "peak_rss_bytes": 1024}
                for name in profiles.PAGE_NAMES
            },
            "mixed": {
                "errors": [],
                "background": {"errors": []},
                "workloads": {"rating": samples, "edit": samples},
            },
            "recovery": {"before": {"actual": 1}, "after": {"actual": 2}},
            "stress_64mib": {
                "error": "diagnostic only",
                "p95_ms": 9000,
                "peak_rss_bytes": 10 * 1024**3,
            },
        }
        self.assertTrue(profiles.profile_checks(result, 4, 4)["all_pass"])
        for mutation in ["warm", "fresh", "writes", "plans"]:
            broken = copy.deepcopy(result)
            if mutation == "warm":
                broken["warm"]["workloads"]["folder"]["distribution"]["p95_ms"] = 101
            elif mutation == "fresh":
                broken["fresh"]["folder"]["peak_rss_bytes"] = profiles.RSS_LIMIT + 1
            elif mutation == "writes":
                broken["mixed"]["workloads"]["edit"]["n"] = 1
            else:
                broken["plans"] = {"error": "EXPLAIN failed"}
            self.assertFalse(
                profiles.profile_checks(broken, 4, 4)["all_pass"], mutation
            )


if __name__ == "__main__":
    unittest.main()

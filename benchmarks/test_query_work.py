"""Bounded diagnostic checks. Execute only after the reference timing lane is released."""

import copy
import json
from pathlib import Path
import sqlite3
import tempfile
import unittest

import duckdb

import catalog_benchmark as bench
import query_work as diagnostic


class MetricValidation(unittest.TestCase):
    def profile(self):
        return {
            "query_name": "SELECT x FROM t",
            "rows_returned": 200,
            "cumulative_rows_scanned": 10000,
            "children": [{
                "operator_name": "TOP_N",
                "operator_type": "TOP_N",
                "operator_cardinality": 200,
                "operator_rows_scanned": 0,
                "children": [{
                    "operator_name": "SEQ_SCAN",
                    "operator_type": "TABLE_SCAN",
                    "operator_cardinality": 5000,
                    "operator_rows_scanned": 10000,
                    "children": [],
                }],
            }],
        }

    def test_duck_profile_rejects_wrong_missing_incomplete_metrics(self):
        valid = self.profile()
        self.assertEqual(diagnostic.validate_duck_profile(valid, "SELECT x FROM t", 200)["operator_rows_scanned_sum"], 10000)
        broken = []
        wrong = copy.deepcopy(valid)
        wrong["query_name"] = "SELECT x FROM other"
        broken.append(wrong)
        missing = copy.deepcopy(valid)
        del missing["children"][0]["children"][0]["operator_rows_scanned"]
        broken.append(missing)
        incomplete = copy.deepcopy(valid)
        incomplete["children"][0]["children"] = []
        broken.append(incomplete)
        invalid = copy.deepcopy(valid)
        invalid["children"][0]["operator_cardinality"] = True
        broken.append(invalid)
        for value in broken:
            with self.subTest(value=value), self.assertRaises(ValueError):
                diagnostic.validate_duck_profile(value, "SELECT x FROM t", 200)

    def test_sqlite_fallback_requires_vm_work_and_complete_scanstatus(self):
        valid = {"vm_step": 9000, "sort": 0, "fullscan_step": 0,
                 "scanstatus_available": False, "scan_loops": [],
                 "scanstatus_unavailable_reason": "not compiled in"}
        diagnostic.validate_sqlite_metrics(valid)
        for change in ({"vm_step": None}, {"vm_step": 0}, {"sort": -1},
                       {"scanstatus_available": True}, {"scanstatus_unavailable_reason": ""}):
            with self.subTest(change=change), self.assertRaises(ValueError):
                diagnostic.validate_sqlite_metrics(valid | change)
        loops = {"nloop": 1, "nvisit": 200, "explain": "SEARCH t USING INTEGER PRIMARY KEY"}
        diagnostic.validate_sqlite_metrics(valid | {"scanstatus_available": True, "scan_loops": [loops]})
        with self.assertRaises(ValueError):
            diagnostic.validate_sqlite_metrics(valid | {"scanstatus_available": True, "scan_loops": [{"nloop": 1, "explain": "SEARCH"}]})

    def test_page_oracle_rejects_skipped_and_wrong_records(self):
        rows = diagnostic.expected_page("page_deep", [500], 1000)
        diagnostic.validate_rows("page_deep", [500], rows, 1000)
        for bad in (rows[:-1], rows[1:] + [rows[-1]], [rows[0][:4] + [(rows[0][4] + 1) % 6, rows[0][5]]] + rows[1:]):
            with self.assertRaises(ValueError):
                diagnostic.validate_rows("page_deep", [500], bad, 1000)


class ReadOnlyEngines(unittest.TestCase):
    @staticmethod
    def fixture(path, engine):
        db = sqlite3.connect(path) if engine == "sqlite" else duckdb.connect(str(path))
        try:
            db.execute("CREATE TABLE assets(sequence BIGINT PRIMARY KEY,id TEXT,folder_id BIGINT,captured_at BIGINT,preview_hash TEXT)")
            db.execute("CREATE TABLE annotations(asset_id BIGINT PRIMARY KEY,rating INTEGER)")
            assets, annotations = [], []
            for sequence in range(1, 1001):
                row = bench.asset_row(sequence)
                assets.append((row[0], row[1], row[9], row[10], row[7]))
                annotations.append((row[0], row[11]))
            db.executemany("INSERT INTO assets VALUES(?,?,?,?,?)", assets)
            db.executemany("INSERT INTO annotations VALUES(?,?)", annotations)
            db.commit()
            if engine == "duckdb":
                db.execute("CHECKPOINT")
        finally:
            db.close()

    def test_native_sqlite_readonly_work_and_source_preservation(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "native.sqlite3"
            self.fixture(path, "sqlite")
            before = diagnostic.source_state(path)
            db = diagnostic.NativeSQLite(path, 16)
            try:
                rows, metrics = db.query(bench.QUERY_SQL["page_deep"], [500], metrics=True)
                diagnostic.validate_rows("page_deep", [500], rows, 1000)
                self.assertGreater(metrics["vm_step"], 0)
                with self.assertRaises(RuntimeError):
                    db.query("DELETE FROM assets")
            finally:
                db.close()
            self.assertEqual(before, diagnostic.source_state(path))

    def test_duckdb_profile_readonly_and_source_preservation(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            path = root / "native.duckdb"
            self.fixture(path, "duckdb")
            before = diagnostic.source_state(path)
            db = duckdb.connect(str(path), read_only=True, config={"memory_limit": "64MiB", "threads": 1, "temp_directory": str(root / "spill")})
            try:
                rows, profile, metrics = diagnostic.duck_query(db, bench.QUERY_SQL["page_deep"], [500], root / "profile.json")
                diagnostic.validate_rows("page_deep", [500], rows, 1000)
                self.assertGreater(metrics["operator_rows_scanned_sum"], 0)
                self.assertEqual(profile["rows_returned"], 200)
                with self.assertRaises(duckdb.Error):
                    db.execute("DELETE FROM assets")
            finally:
                db.close()
            self.assertEqual(before, diagnostic.source_state(path))

    def test_exclusive_output_and_failed_source_proof_leave_receipt(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "snapshot"
            relative = "1000/sqlite/catalog.sqlite3"
            path = source / relative
            path.parent.mkdir(parents=True)
            self.fixture(path, "sqlite")
            before = diagnostic.source_state(path)
            manifest = {"complete": True, "counts": [1000],
                        "frozen_harness_sha256": diagnostic.FROZEN_SHA256,
                        "snapshots": {relative: {"sha256": "incorrect", "bytes": before["bytes"]}}}
            (source / "snapshot.json").write_text(json.dumps(manifest))
            output = root / "receipt"
            with self.assertRaisesRegex(ValueError, "differs from preserved snapshot"):
                diagnostic.run(source, "sqlite", 1000, 16, output)
            receipt = json.loads((output / "receipt.json").read_text())
            self.assertFalse(receipt["complete"])
            self.assertTrue(receipt["source_preserved"])
            self.assertEqual(receipt["queries"], [])
            with self.assertRaises(FileExistsError):
                diagnostic.run(source, "sqlite", 1000, 16, output)
            self.assertEqual(before, diagnostic.source_state(path))


if __name__ == "__main__":
    unittest.main()

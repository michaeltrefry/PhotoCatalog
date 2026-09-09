"""Bounded diagnostic checks. Execute only after the reference timing lane is released."""

import copy
import hashlib
import json
from pathlib import Path
import sqlite3
import tempfile
import unittest
from unittest.mock import patch

import duckdb

import catalog_benchmark as bench
import query_work as diagnostic


class QueryCases(unittest.TestCase):
    def test_original_cursors_are_retained_and_iteration_nine_is_distinct(self):
        self.assertEqual(diagnostic.VERSION, 3)
        for count in diagnostic.SCALES:
            cases = diagnostic.query_cases(count)
            self.assertEqual(len(cases), 6)
            self.assertEqual(len({(case["workload"], case["case_label"]) for case in cases}), 6)
            rating = bench.query_parameters("rating", count, 0)[0]
            expected = []
            for percent in (50, 90):
                for name in ("page_deep", "rating"):
                    cursor = count * percent // 100
                    expected.append({
                        "workload": name,
                        "case_label": f"cursor_{percent}_percent",
                        "iteration": None,
                        "cursor_percent": percent,
                        "parameters": [cursor] if name == "page_deep" else [rating, cursor],
                    })
            self.assertEqual(cases[:4], expected)
            for case in cases[4:]:
                self.assertEqual(case["case_label"], "frozen_iteration_9")
                self.assertEqual(case["iteration"], 9)
                self.assertEqual(case["parameters"], bench.query_parameters(case["workload"], count, 9))
                self.assertEqual(case["cursor_percent"], case["parameters"][-1] * 100 / count)
                self.assertNotEqual(case["parameters"][-1], count * 90 // 100)

    def test_iteration_nine_does_not_substitute_cursor_or_rating(self):
        # Alternate generator outputs catch implementations that hard-code a
        # nominal 90.5% cursor or reuse the legacy cases' iteration-0 rating.
        def parameters(name, count, iteration):
            self.assertEqual(count, 1_000_000)
            if iteration == 0:
                self.assertEqual(name, "rating")
                return [2, 500_000]
            self.assertEqual(iteration, 9)
            return [314_159] if name == "page_deep" else [5, 271_828]

        with patch.object(bench, "query_parameters", side_effect=parameters) as frozen:
            cases = diagnostic.query_cases(1_000_000)
        self.assertEqual(cases[4]["parameters"], [314_159])
        self.assertEqual(cases[5]["parameters"], [5, 271_828])
        self.assertEqual(frozen.call_count, 3)


class QuerySelection(unittest.TestCase):
    def test_default_and_explicit_baseline_keep_exact_frozen_sql(self):
        before = copy.deepcopy(bench.QUERY_SQL)
        expected = {name: before[name] for name in diagnostic.WORKLOADS}
        expected_digest = hashlib.sha256(json.dumps(
            expected, sort_keys=True, separators=(",", ":"), ensure_ascii=False
        ).encode("utf-8")).hexdigest()
        for engine in ("sqlite", "duckdb"):
            selected = diagnostic.query_selection(engine)
            self.assertEqual(selected, diagnostic.query_selection(engine, "baseline"))
            self.assertEqual(selected["variant"], "baseline")
            self.assertEqual(selected["sql_map"], expected)
            self.assertEqual(selected["sql_map_sha256"], expected_digest)
            self.assertIsNone(selected["candidate_source_sha256"])
            selected["sql_map"]["page_deep"] = "local change"
        self.assertEqual(bench.QUERY_SQL, before)
        self.assertEqual(diagnostic.query_selection("sqlite")["sql_map"], expected)

    def test_candidate_records_source_and_sql_without_global_mutation(self):
        import query_candidates

        frozen_before = copy.deepcopy(bench.QUERY_SQL)
        candidate_before = copy.deepcopy(query_candidates.CANDIDATE_SQL)
        with patch.object(diagnostic, "digest", return_value="a" * 64) as digest:
            selected = diagnostic.query_selection("sqlite", "sqlite_page_candidate")
            digest.assert_called_once_with(query_candidates.__file__)
        self.assertEqual(selected["variant"], "sqlite_page_candidate")
        self.assertEqual(selected["sql_map"], candidate_before)
        self.assertEqual(selected["candidate_source_sha256"], "a" * 64)
        self.assertEqual(selected["sql_map_sha256"], hashlib.sha256(json.dumps(
            candidate_before, sort_keys=True, separators=(",", ":"), ensure_ascii=False
        ).encode("utf-8")).hexdigest())
        self.assertNotEqual(selected["sql_map_sha256"], diagnostic.query_selection("sqlite")["sql_map_sha256"])
        selected["sql_map"]["rating"] = "local change"
        self.assertEqual(query_candidates.CANDIDATE_SQL, candidate_before)
        self.assertEqual(bench.QUERY_SQL, frozen_before)

    def test_cli_defaults_and_explicit_candidate_dispatch(self):
        common = ["query_work.py", "--snapshot", "unused-snapshot", "--engine", "sqlite",
                  "--count", "1000000", "--memory-mb", "256", "--output", "unused-output"]
        for extra, variant in (([], "baseline"), (["--variant", "sqlite_page_candidate"], "sqlite_page_candidate")):
            with self.subTest(variant=variant), patch.object(diagnostic.sys, "argv", common + extra), \
                 patch.object(diagnostic, "run") as run:
                diagnostic.main()
                run.assert_called_once_with(Path("unused-snapshot"), "sqlite", 1000000, 256,
                                            Path("unused-output"), variant=variant)
        bad = common.copy()
        bad[bad.index("sqlite")] = "duckdb"
        with patch.object(diagnostic.sys, "argv", bad + ["--variant", "sqlite_page_candidate"]), \
             patch.object(diagnostic, "run") as run, \
             patch.object(diagnostic.sys, "stderr"):
            with self.assertRaises(SystemExit):
                diagnostic.main()
            run.assert_not_called()

    def test_invalid_selection_is_rejected_before_source_or_database_access(self):
        for engine, variant in (("duckdb", "sqlite_page_candidate"), ("sqlite", "unknown")):
            with self.subTest(engine=engine, variant=variant), \
                 patch.object(diagnostic.Path, "read_text") as read_source, \
                 patch.object(diagnostic, "source_state") as state, \
                 patch.object(diagnostic, "digest") as digest:
                with self.assertRaises(ValueError):
                    diagnostic.run("unused-snapshot", engine, 1000, 16, "unused-output", variant=variant)
                read_source.assert_not_called()
                state.assert_not_called()
                digest.assert_not_called()


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
            self.assertEqual(receipt["variant"], "baseline")
            self.assertEqual(receipt["sql_map"], {name: bench.QUERY_SQL[name] for name in diagnostic.WORKLOADS})
            self.assertEqual(receipt["sql_map_sha256"], diagnostic.query_selection("sqlite")["sql_map_sha256"])
            self.assertIsNone(receipt["candidate_source_sha256"])
            self.assertTrue(receipt["source_preserved"])
            self.assertEqual(receipt["queries"], [])
            with self.assertRaises(FileExistsError):
                diagnostic.run(source, "sqlite", 1000, 16, output)
            self.assertEqual(before, diagnostic.source_state(path))


if __name__ == "__main__":
    unittest.main()

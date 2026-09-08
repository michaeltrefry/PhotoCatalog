"""Small deterministic correctness tests; no timing assertions or private fixtures."""

import json
from pathlib import Path
import tempfile
import unittest

import catalog_benchmark as bench


class BenchmarkContract(unittest.TestCase):
    def test_logical_rows_and_query_results_match_independent_filter(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "csv"
            count = 2000
            bench.generate(source, count)
            logical = [bench.asset_row(i) for i in range(1, count + 1)]
            for engine in ["sqlite", "duckdb"]:
                path = root / engine / "catalog.db"
                receipt = bench.load(engine, path, source, 64)
                self.assertEqual(receipt["proof"]["count"], count)
                db = bench.connect(engine, path, 64)
                for name, sql in bench.QUERY_SQL.items():
                    if name == "aggregate":
                        continue
                    args = bench.query_parameters(name, count, 0)

                    def matches(row):
                        sequence, _, _, _, _, _, _, _, _, folder, date, rating, _, _ = (
                            row
                        )
                        if name == "page_deep":
                            return sequence > args[0]
                        if name == "folder":
                            return folder == args[0] and sequence > args[1]
                        if name == "date":
                            return args[0] <= date < args[1]
                        if name == "rating":
                            return rating == args[0] and sequence > args[1]
                        if name == "keyword":
                            return (
                                any(
                                    keyword == args[0]
                                    for _, keyword in bench.keywords(sequence)
                                )
                                and sequence > args[1]
                            )
                        if name == "collection":
                            return (args[0], sequence) in bench.collections(
                                sequence
                            ) and sequence > args[1]
                        return (
                            folder == args[0]
                            and rating == args[1]
                            and args[2] <= date < args[3]
                            and sequence > args[4]
                        )

                    expected = sorted(
                        (row for row in logical if matches(row)),
                        key=lambda row: (row[10], row[0]) if name == "date" else row[0],
                    )[:200]
                    expected = [
                        (r[0], r[1], r[9], r[10], r[11], r[7]) for r in expected
                    ]
                    actual = db.execute(sql, args).fetchall()
                    self.assertEqual(actual, expected, (engine, name))
                    bench.validate_page(name, args, actual)
                self.assertEqual(
                    bench.measured_settings(db, engine)["fullfsync"], 1
                ) if engine == "sqlite" else None
                db.close()

    def test_summary_fails_closed_for_plan_and_fresh_memory_proof(self):
        import copy

        samples = bench.distribution([1, 1, 1, 1])
        warm = {
            "peak_rss_bytes": 1024,
            "workloads": {name: {"distribution": samples} for name in bench.QUERY_SQL},
        }
        fresh = {
            name: {"errors": [], "open_plus_query": samples, "peak_rss_bytes": 1024}
            for name in bench.QUERY_SQL
            if name != "aggregate"
        }
        good = {
            "load": {},
            "plans": {name: [[0, "plan"]] for name in bench.QUERY_SQL},
            "warm_256mb": warm,
            "warm_64mb": warm,
            "fresh_process": fresh,
            "mixed": {
                "errors": [],
                "background": {"errors": []},
                "workloads": {"rating": samples, "edit": samples},
            },
            "recovery": {"before": {"actual": 1}, "after": {"actual": 2}},
        }
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bench.dump(
                root / "campaign.json",
                {"counts": [10000], "repetitions": 4, "fresh_repetitions": 4},
            )

            def evaluate(receipt):
                for engine in ["sqlite", "duckdb"]:
                    bench.dump(root / "10000" / f"{engine}.json", receipt)
                return bench.summarize(root)["scale_results"]["10000"]["sqlite"]

            self.assertTrue(evaluate(good)["all_pass"])
            broken = copy.deepcopy(good)
            broken["plans"] = {"error": "EXPLAIN failed", "returncode": 1}
            self.assertFalse(evaluate(broken)["checks"]["query_plans"])
            broken = copy.deepcopy(good)
            del broken["plans"]["page_deep"]
            self.assertFalse(evaluate(broken)["all_pass"])
            broken = copy.deepcopy(good)
            broken["fresh_process"]["page_deep"]["peak_rss_bytes"] = 8 * 1024**3
            self.assertFalse(evaluate(broken)["checks"]["fresh_rss"])
            broken = copy.deepcopy(good)
            del broken["fresh_process"]["page_deep"]["peak_rss_bytes"]
            self.assertFalse(evaluate(broken)["all_pass"])
            broken = copy.deepcopy(good)
            broken["mixed"]["workloads"]["rating"]["n"] = 1
            self.assertFalse(evaluate(broken)["checks"]["mixed_writes"])

    def test_generator_is_reproducible_and_metadata_agrees(self):
        a = bench.asset_row(17)
        b = bench.asset_row(18)
        self.assertEqual(a, bench.asset_row(17))
        self.assertNotEqual(a[6], b[6])
        metadata = json.loads(a[6])
        self.assertEqual(metadata["captured_at"], bench.iso_date(a[10]))
        self.assertIn(str(a[12]), metadata["camera_model"])
        self.assertEqual(len({bench.asset_row(i)[1] for i in range(1, 1001)}), 1000)

    def test_sparse_pages_cannot_establish_scale_page_budget(self):
        with self.assertRaises(AssertionError):
            bench.validate_page("page_deep", [500000], [], 1000000)
        with self.assertRaises(AssertionError):
            bench.validate_page("page_deep", [1], [(1, "invalid", 0, 0, 0, "")])

    def test_interrupted_transactions_keep_only_acknowledged_commit(self):
        with tempfile.TemporaryDirectory() as directory:
            for engine in ["sqlite", "duckdb"]:
                path = Path(directory) / (engine + ".db")
                db = bench.connect(engine, path, 64)
                bench.create_schema(db, engine)
                db.close()
                result = bench.recovery(engine, path, 64)
                self.assertEqual(result["before"]["actual"], 1)
                self.assertEqual(result["after"]["actual"], 2)


if __name__ == "__main__":
    unittest.main()

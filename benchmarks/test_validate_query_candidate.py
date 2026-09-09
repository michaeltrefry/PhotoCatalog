"""Small candidate experiment tests; execute only after timing-lane release."""

import copy
import hashlib
from pathlib import Path
import sqlite3
import tempfile
import unittest
from unittest.mock import patch

import catalog_benchmark as bench
import query_candidates
import validate_query_candidate as runner


class CandidateChild(unittest.TestCase):
    def test_subprocess_executes_actual_candidate_sql_and_reports_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            dbpath = root / "fixture.sqlite3"
            db = sqlite3.connect(dbpath)
            bench.create_schema(db, "sqlite")
            bench.insert_batch(db, [bench.asset_row(i) for i in range(1, 1001)], bulk=True)
            bench.add_indexes(db)
            db.commit()
            db.close()
            original_sql = bench.QUERY_SQL.copy()
            for name in ("page_deep", "rating"):
                req = runner.request("read", 1000, 1, name, 0)
                result = runner.child(dbpath, req, root / (name + ".json"), trace_sql=True)
                self.assertTrue(runner.valid_envelope(result, runner.identity(), req), result)
                trace = [" ".join(sql.split()) for sql in result["executed_sql_trace"]]
                fragment = "FROM assets a CROSS JOIN annotations r" if name == "page_deep" else "FROM annotations r CROSS JOIN assets a"
                self.assertTrue(any(fragment in sql for sql in trace), trace)
                self.assertEqual(result["identity"]["candidate_sql"][name], query_candidates.CANDIDATE_SQL[name])
                self.assertNotIn("aggregate", result["result"]["workloads"])
                self.assertEqual(result["parameter_schedule"][name][0]["parameters"], bench.query_parameters(name, 1000, 0))
            self.assertEqual(bench.QUERY_SQL, original_sql)
            self.assertEqual(Path(bench.__file__).resolve(), runner.FROZEN_FILE)

    def test_recovery_dispatch_uses_candidate_runner_and_restores_globals(self):
        # The frozen recovery function resolves __file__ at runtime. Verify the
        # exact command it creates rather than merely checking an envelope label.
        class StopAfterDispatch(Exception):
            pass
        original_sql, original_file = bench.QUERY_SQL, bench.__file__
        with patch.object(bench.subprocess, "Popen", side_effect=StopAfterDispatch) as start:
            with self.assertRaises(StopAfterDispatch), runner.candidate_environment():
                bench.recovery("sqlite", Path("fixture.sqlite3"), 256)
        command = start.call_args.args[0]
        self.assertEqual(command[1], str(runner.SCRIPT))
        self.assertEqual(command[2], "crash-worker")
        self.assertIs(bench.QUERY_SQL, original_sql)
        self.assertEqual(bench.__file__, original_file)

    def test_existing_outputs_prevent_child_execution_and_source_access(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "already"
            output.mkdir()
            receipt = output / "prior.json"
            receipt.write_text("preserve this receipt")
            with patch.object(runner.subprocess, "run") as execute:
                with self.assertRaises(FileExistsError):
                    runner.child(Path("must-not-open.sqlite3"), runner.request("mixed", 1000), receipt)
                execute.assert_not_called()
            with patch.object(runner, "identity") as identify:
                with self.assertRaisesRegex(ValueError, "already exists"):
                    runner.run(Path(temporary) / "snapshot", Path(temporary) / "baseline", output)
                identify.assert_not_called()
            self.assertEqual(receipt.read_text(), "preserve this receipt")


class CandidateEvidence(unittest.TestCase):
    def fixture(self):
        ident = runner.identity()
        count, repetitions, fresh_repetitions = 1_000_000, 2, 2
        baseline = {"load": {"proof": {"count": count}}, "warm_256mb": {"workloads": {}}, "fresh_process": {}}

        def correctness(name, iterations):
            return [{"iteration": i, "rows": 200, "sha256": hashlib.sha256(f"{name}:{i}".encode()).hexdigest()} for i in iterations]

        def reading(names, iterations):
            return {"open_ms": 1.0, "peak_rss_bytes": 1024 * 1024,
                    "workloads": {name: {"distribution": bench.distribution([1.0] * len(iterations)), "correctness": correctness(name, iterations)} for name in names}}

        def envelope(req, result):
            value = {"identity": ident, "request": req, "returncode": 0,
                     "settings_actual": ident["settings_requested"], "result": result}
            if req["operation"] == "read":
                value["parameter_schedule"] = {
                    name: [{"iteration": item["iteration"], "parameters": bench.query_parameters(name, count, item["iteration"])} for item in data["correctness"]]
                    for name, data in result["workloads"].items()}
            return value

        warm = reading(runner.PAGES, [0, 1])
        baseline["warm_256mb"] = copy.deepcopy(warm)
        result = {"copy": {"proof": baseline["load"]["proof"], "sha256": "a" * 64},
                  "plans": envelope(runner.request("plans", count), {name: [["plan"]] for name in runner.PAGES}),
                  "warm": envelope(runner.request("read", count, repetitions), warm), "fresh": {}}
        for name in runner.PAGES:
            raw = [reading([name], [i]) for i in range(fresh_repetitions)]
            baseline["fresh_process"][name] = {"raw": copy.deepcopy(raw)}
            result["fresh"][name] = [envelope(runner.request("read", count, 1, name, i), data) for i, data in enumerate(raw)]
        result["mixed"] = envelope(runner.request("mixed", count, repetitions * 2), {
            "peak_rss_bytes": 1024 * 1024,
            "errors": [], "background": {"errors": [], "batches": 2, "rows": 64, "samples_ms": [2.0, 2.0]},
            "concurrent_starts": 4,
            "workloads": {name: bench.distribution([1.0] * (4 if name == "page_during_import" else 2)) for name in ("rating", "edit", "page_during_import")}})
        result["recovery"] = envelope(runner.request("recovery", count), {
            stage: {"expected": expected, "actual": expected, "forced_exit_code": -9}
            for stage, expected in (("before", 1), ("after", 2))})
        return result, baseline, ident, count, repetitions, fresh_repetitions

    def test_complete_page_matrix_and_matching_hashes_are_required(self):
        args = self.fixture()
        self.assertTrue(runner.evaluate(*args)["all_pass"])
        mutations = [
            lambda r: r["warm"].update(error="child failure"),
            lambda r: r["plans"].update(returncode=1),
            lambda r: r["warm"]["result"]["workloads"].pop("collection"),
            lambda r: r["fresh"]["rating"].pop(),
            lambda r: r["fresh"]["rating"][0]["parameter_schedule"]["rating"][0].update(parameters=[5, 0]),
            lambda r: r["warm"]["result"]["workloads"]["rating"]["correctness"][0].update(sha256="wrong"),
            lambda r: r["mixed"]["result"]["workloads"]["edit"].update(n=1),
            lambda r: r["mixed"]["result"].update(errors=["OutOfMemoryException"]),
            lambda r: r["recovery"]["result"]["before"].update(forced_exit_code=0),
            lambda r: r["warm"]["result"].update(peak_rss_bytes=8 * 1024**3),
            lambda r: r["fresh"]["rating"][0]["result"].update(peak_rss_bytes=8 * 1024**3),
            lambda r: r["mixed"]["result"].pop("peak_rss_bytes"),
            lambda r: r["mixed"]["result"].update(peak_rss_bytes=0),
        ]
        for mutate in mutations:
            with self.subTest(mutation=mutate):
                changed = copy.deepcopy(args)
                mutate(changed[0])
                self.assertFalse(runner.evaluate(*changed)["all_pass"])

    def test_mixed_rss_is_recorded_without_a_browse_budget_gate(self):
        args = self.fixture()
        args[0]["mixed"]["result"]["peak_rss_bytes"] = 8 * 1024**3
        evaluation = runner.evaluate(*args)
        self.assertTrue(evaluation["checks"]["mixed_rss_recorded"])
        self.assertTrue(evaluation["all_pass"])
        self.assertEqual(args[0]["mixed"]["result"]["peak_rss_bytes"], 8 * 1024**3)

    def test_stale_child_sql_identity_cannot_pass_matching_results(self):
        args = list(copy.deepcopy(self.fixture()))
        # Preserve correct result hashes but claim the original query in one
        # otherwise-successful child: source identity must reject it.
        args[0]["warm"]["identity"] = copy.deepcopy(args[2])
        args[0]["warm"]["identity"]["candidate_sql"]["page_deep"] = runner.BASE_SQL["page_deep"]
        self.assertFalse(runner.evaluate(*args)["all_pass"])

    def test_distributions_reject_partial_and_forged_quantiles(self):
        valid = bench.distribution([1.0, 2.0])
        self.assertTrue(runner.valid_distribution(valid, 2, 100))
        for change in ({"n": 1}, {"samples_ms": [1.0]}, {"p95_ms": 0}, {"samples_ms": [float("nan"), 2.0]}):
            self.assertFalse(runner.valid_distribution(valid | change, 2, 100))


if __name__ == "__main__":
    unittest.main()

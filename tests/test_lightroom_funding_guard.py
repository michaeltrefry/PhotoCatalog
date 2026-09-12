import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]


def module(path, name):
    spec = importlib.util.spec_from_file_location(name, path)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


GUARD = module(ROOT / "scripts/lightroom_funding_guard.py", "funding_guard_test")
RUN = module(ROOT / "scripts/run_lightroom_inspection.py", "funding_frozen_test")


def reference(path):
    return {"path": str(path), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}


class FundingTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name).resolve()  # macOS /var alias is not evidence.
        self.run = self.root / "run"
        self.attempt = self.root / "attempt"
        self.run.mkdir(); self.attempt.mkdir()
        self.policy = {"run": str(self.run), "attempt": str(self.attempt),
                       "protected_bytes": 100, "operating_reserve_bytes": 20,
                       "command_buffer_bytes": 30, "initial_minimum_bytes": 150}

    def tearDown(self):
        self.temp.cleanup()

    def instance(self, free):
        monitor = GUARD.FundingMonitor(self.policy, "adapter", free=free)
        cls = GUARD.guarded_type(RUN.Runner, monitor, RUN.PauseRequested)
        runner = cls.__new__(cls)
        runner.root = self.run
        runner.config = {"minimum_free_bytes": 10}
        runner.generation = None
        runner.result = lambda value: value
        (self.run / "steps").mkdir()
        return runner, monitor

    def test_real_frozen_call_low_disk_never_reserves_or_spawns(self):
        runner, monitor = self.instance(lambda: 149)
        journal = self.run / "journal.json"
        journal.write_bytes(b'{"next_command":1}\n')
        with mock.patch.object(RUN.subprocess, "Popen", side_effect=AssertionError("unexpected spawn")):
            with self.assertRaises(RUN.PauseRequested):
                runner.call(["next"], ["rows", "dummy"])
        self.assertEqual(journal.read_bytes(), b'{"next_command":1}\n')
        self.assertEqual(list((self.run / "steps").iterdir()), [])
        self.assertFalse((self.run / "commands").exists())
        pause = json.loads((self.run / "pause-request").read_bytes())
        self.assertEqual(pause["owner"], str(self.attempt))
        monitor.finish()
        self.assertIn("command-boundary", json.loads((self.attempt / "funding-adapter-result.json").read_bytes())["stop_reason"])

    def test_completed_page_replay_does_not_become_new_execution(self):
        runner, monitor = self.instance(lambda: 0)
        key = ["already", 24]
        record = {"requested_arguments": ["rows", "dummy"], "marker": "retained"}
        (self.run / "saved.json").write_bytes(GUARD.encoded(record))
        (self.run / "steps" / (RUN.sha(RUN.encoded(key)) + ".json")).write_bytes(GUARD.encoded({"record": "saved.json"}))
        self.assertEqual(runner.call(key, ["rows", "dummy"]), record)
        self.assertEqual(monitor.samples, 0)
        self.assertFalse((self.run / "pause-request").exists())

    def test_existing_larger_allocation_admission_preserved(self):
        monitor = GUARD.FundingMonitor(self.policy, "adapter", free=lambda: 190)
        with self.assertRaises(RUN.PauseRequested):
            monitor.boundary(200, RUN.PauseRequested)

    def test_outer_pause_then_emergency_and_durable_minima(self):
        values = iter([170, 160, 149, 121, 120])
        monitor = GUARD.FundingMonitor(self.policy, "outer", free=lambda: next(values))
        monitor.outer(); monitor.outer(); monitor.outer(); monitor.outer()
        pause = (self.run / "pause-request").read_bytes()
        with self.assertRaisesRegex(RuntimeError, "emergency"):
            monitor.outer()
        self.assertEqual((self.run / "pause-request").read_bytes(), pause)
        monitor.finish()
        result = json.loads((self.attempt / "funding-outer-result.json").read_bytes())
        self.assertEqual((result["initial_free_bytes"], result["minimum_observed_free_bytes"], result["samples"]), (170, 120, 5))
        self.assertEqual(len(list(self.attempt.glob("funding-outer-minimum-*.json"))), 5)
        self.assertIn("not a quota", (self.attempt / "funding-outer-minimum-00000005.json").read_text())

    def test_existing_pause_never_overwritten(self):
        (self.run / "pause-request").write_bytes(b'foreign immutable evidence')
        monitor = GUARD.FundingMonitor(self.policy, "outer", free=lambda: 140)
        monitor.outer()
        self.assertEqual((self.run / "pause-request").read_bytes(), b'foreign immutable evidence')

    def test_failure_evidence_cannot_silently_retry_same_attempt(self):
        first = GUARD.FundingMonitor(self.policy, "outer", free=lambda: 170)
        first.outer(); first.finish()
        retry = GUARD.FundingMonitor(self.policy, "outer", free=lambda: 170)
        with self.assertRaises(FileExistsError):
            retry.outer()

    def prepared_policy(self):
        runner = self.root / "frozen.py"
        helper = self.root / "lightroom_generation.py"
        marker = self.root / "imported"
        runner.write_text("from pathlib import Path\nPath(" + repr(str(marker)) + ").touch()\n")
        helper.write_text("# trusted synthetic helper\n")
        config = self.run / "config.json"
        config.write_bytes(GUARD.encoded({"exclusive_output": str(self.run)}))
        binding = self.run / "binding.json"
        binding.write_bytes(GUARD.encoded({"driver_sha256": reference(runner)["sha256"],
            "generation_driver_sha256": reference(helper)["sha256"], "config_sha256": reference(config)["sha256"],
            "source": "synthetic-core", "binary_sha256": "synthetic-native-digest"}))
        policy = dict(self.policy, protocol=1, adapter=reference(Path(GUARD.__file__)),
                      runner=reference(runner), helper=reference(helper), config=reference(config),
                      binding=reference(binding), native_core="synthetic-core", native_binary_sha256="synthetic-native-digest")
        path = self.root / "policy.json"
        path.write_bytes(GUARD.encoded(policy))
        return path, policy, marker

    def test_every_source_and_config_is_checked_before_import(self):
        path, policy, marker = self.prepared_policy()
        for name in ["runner", "helper", "config", "binding", "adapter"]:
            with self.subTest(name=name):
                changed = json.loads(json.dumps(policy))
                changed[name]["sha256"] = "0" * 64
                path.write_bytes(GUARD.encoded(changed))
                with self.assertRaisesRegex(ValueError, "digest mismatch"):
                    GUARD.execute(path, reference(path)["sha256"])
                self.assertFalse(marker.exists())
        with self.assertRaisesRegex(ValueError, "digest mismatch"):
            GUARD.execute(path, "0" * 64)

    def test_zero_protected_reserve_preserves_exact_main_admission(self):
        path, policy, marker = self.prepared_policy()
        policy.update(protected_bytes=0, operating_reserve_bytes=34359738368,
                      command_buffer_bytes=15132975104 * 12,
                      initial_minimum_bytes=215955439616)
        path.write_bytes(GUARD.encoded(policy))
        checked, _ = GUARD.load_policy(path, reference(path)["sha256"])
        self.assertEqual(checked, policy)
        self.assertFalse(marker.exists())
        values = iter([215955439616, 215955439615, 34359738368])
        monitor = GUARD.FundingMonitor(checked, "adapter", free=lambda: next(values))
        self.assertEqual(monitor.floor, 34359738368)
        self.assertEqual(monitor.trigger, 215955439616)
        self.assertEqual(monitor.boundary(34359738368, RUN.PauseRequested),
                         {"available_bytes": 215955439616, "required_bytes": 215955439616})
        with self.assertRaises(RUN.PauseRequested):
            monitor.boundary(34359738368, RUN.PauseRequested)
        with self.assertRaisesRegex(RuntimeError, "emergency"):
            monitor.outer()
        monitor.finish()
        result = json.loads((self.attempt / "funding-adapter-result.json").read_bytes())
        self.assertEqual(result["protected_bytes"], 0)
        self.assertEqual(result["operating_reserve_bytes"], 34359738368)

    def test_negative_protected_and_nonpositive_other_reserves_rejected(self):
        path, policy, marker = self.prepared_policy()
        for field, bad in [("protected_bytes", -1), ("protected_bytes", False),
                           ("protected_bytes", 0.0), ("operating_reserve_bytes", 0),
                           ("command_buffer_bytes", 0), ("initial_minimum_bytes", 0)]:
            with self.subTest(field=field, bad=bad):
                changed = dict(policy); changed[field] = bad
                path.write_bytes(GUARD.encoded(changed))
                with self.assertRaisesRegex(ValueError, "unresolved funding amount"):
                    GUARD.load_policy(path, reference(path)["sha256"])
                self.assertFalse(marker.exists())

    def test_policy_native_binding_and_arithmetic_not_unchecked_claims(self):
        path, policy, marker = self.prepared_policy()
        for field, bad in [("native_core", "other-core"), ("native_binary_sha256", "other-native"),
                           ("initial_minimum_bytes", 149), ("protected_bytes", True)]:
            with self.subTest(field=field):
                changed = dict(policy); changed[field] = bad
                path.write_bytes(GUARD.encoded(changed))
                with self.assertRaises(ValueError):
                    GUARD.execute(path, reference(path)["sha256"])
                self.assertFalse(marker.exists())

    def test_only_main_dispatch_and_inherited_runner_method_changed(self):
        path, policy, marker = self.prepared_policy()
        frozen = Path(policy["runner"]["path"])
        frozen.write_text('''import sys
class PauseRequested(Exception): pass
class Runner:
    def untouched(self): return "inherited"
    def space(self, minimum=None): raise AssertionError("unguarded space")
def main():
    assert sys.argv == [__file__, "main", ''' + repr(str(self.run)) + ''']
    runner = Runner(); runner.config = {"minimum_free_bytes": 10}
    assert runner.untouched() == "inherited"
    runner.space()
''')
        policy["runner"] = reference(frozen)
        binding_path = Path(policy["binding"]["path"])
        binding = json.loads(binding_path.read_bytes()); binding["driver_sha256"] = reference(frozen)["sha256"]
        binding_path.write_bytes(GUARD.encoded(binding)); policy["binding"] = reference(binding_path)
        path.write_bytes(GUARD.encoded(policy))
        with mock.patch.object(GUARD.FundingMonitor, "available", return_value=149):
            with self.assertRaisesRegex(Exception, "command-boundary"):
                GUARD.execute(path, reference(path)["sha256"])
        result = json.loads((self.attempt / "funding-adapter-result.json").read_bytes())
        self.assertEqual(result["minimum_observed_free_bytes"], 149)
        self.assertFalse(marker.exists())


if __name__ == "__main__":
    unittest.main()

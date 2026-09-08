import json
import plistlib
import signal
import tempfile
from pathlib import Path
from unittest import mock
import subprocess
import types
import unittest

from observe_host import counter_delta, gpu_snapshot, main, stamp


class ObserverTests(unittest.TestCase):
    def test_existing_output_is_never_overwritten(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "existing.jsonl"
            output.write_text("preserved evidence")
            with self.assertRaises(FileExistsError):
                main(["--output", str(output), "--duration-seconds", "1"])
            self.assertEqual(output.read_text(), "preserved evidence")

    def test_signal_handler_writes_final_record_and_restores_previous_handler(self):
        for signum in (signal.SIGINT, signal.SIGTERM):
            with self.subTest(signum=signum), tempfile.TemporaryDirectory() as directory:
                output = Path(directory) / "signal.jsonl"
                previous = signal.getsignal(signum)
                def signal_during_sample():
                    # Invoke the installed handler without signaling unrelated workloads.
                    signal.getsignal(signum)(signum, None)
                    return {"kind": "sample", **stamp()}
                with mock.patch("observe_host.HostSampler") as sampler:
                    sampler.return_value.sample.side_effect = signal_during_sample
                    self.assertEqual(main(["--output", str(output), "--duration-seconds", "1"]), 0)
                records = [json.loads(line) for line in output.read_text().splitlines()]
                self.assertEqual([r["kind"] for r in records], ["start", "sample", "final"])
                self.assertEqual(records[-1]["reason"], signal.Signals(signum).name)
                self.assertEqual(records[-1]["samples"], 1)
                self.assertEqual(signal.getsignal(signum), previous)

    def test_counter_deltas_are_not_invented_for_missing_baseline_or_reset(self):
        self.assertEqual(counter_delta(None, {"read_bytes": 5})["status"], "unavailable")
        self.assertEqual(counter_delta({"read_bytes": 5}, None)["status"], "baseline")
        self.assertEqual(counter_delta({"read_bytes": 9}, {"read_bytes": 5}),
                         {"status": "available", "values": {"read_bytes": 4}})
        self.assertEqual(counter_delta({"read_bytes": 3}, {"read_bytes": 5})["status"], "reset")
        self.assertEqual(counter_delta({"new_counter": 3}, {"read_bytes": 5})["status"], "unavailable")

    def test_gpu_missing_is_unavailable_not_zero(self):
        def absent(*args, **kwargs):
            return types.SimpleNamespace(returncode=0, stdout=plistlib.dumps([]))
        self.assertEqual(gpu_snapshot("Darwin", absent)["status"], "unavailable")
        self.assertEqual(gpu_snapshot("Linux", absent)["status"], "unavailable")

    def test_gpu_timeout_and_malformed_output_are_explicit(self):
        def timeout(*args, **kwargs):
            raise subprocess.TimeoutExpired("ioreg", 3)
        self.assertEqual(gpu_snapshot("Darwin", timeout)["reason"], "TimeoutExpired")
        def malformed(*args, **kwargs):
            return types.SimpleNamespace(returncode=0, stdout=b"not plist")
        self.assertEqual(gpu_snapshot("Darwin", malformed)["status"], "error")

    def test_gpu_partial_fields_and_observed_zero_are_distinct(self):
        def partial(*args, **kwargs):
            return types.SimpleNamespace(returncode=0, stdout=plistlib.dumps([
                {"PerformanceStatistics": {"Device Utilization %": 0,
                                           "Alloc system memory": 1024}}]))
        snapshot = gpu_snapshot("Darwin", partial)
        self.assertEqual(snapshot["status"], "partial")
        self.assertEqual(snapshot["devices"][0]["device_utilization_percent"], 0)
        self.assertIsNone(snapshot["devices"][0]["in_use_system_memory_bytes"])
        self.assertEqual(snapshot["devices"][0]["allocated_system_memory_bytes"], 1024)


if __name__ == "__main__":
    unittest.main()

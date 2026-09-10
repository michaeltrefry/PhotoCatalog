"""Tiny observer binding checks; no rendering or timing campaign."""
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import preview_host


class HostBindingTests(unittest.TestCase):
    def test_initial_unavailable_gpu_is_retained_and_receipt_binds_bytes(self):
        sample={"kind":"sample","gpu":{"status":"unavailable","reason":"fixture"},"cpu":{"status":"baseline"}}
        with tempfile.TemporaryDirectory() as root, patch.object(preview_host.observer.HostSampler, "sample", return_value=sample):
            with preview_host.HostObservation(root) as observation:
                # Persisted before caller can start any measured child.
                rows=[json.loads(x) for x in (Path(root)/'host.jsonl').read_text().splitlines()]
                self.assertEqual(rows[1]['gpu']['status'],'unavailable')
                result=observation.finish()
                self.assertTrue(result['complete'])  # Recording completed; this is not a quiet-host pass.
                self.assertIn('not evaluated',result['quietness'])
            self.assertEqual(result['sha256'],preview_host.sha(Path(root)/'host.jsonl'))
            self.assertEqual(result,observation.finish())
            self.assertEqual(json.loads((Path(root)/'host-receipt.json').read_text()),result)
            with self.assertRaises(FileExistsError):
                with preview_host.HostObservation(root):
                    self.fail('overwrote host evidence')


    def test_oversized_initial_host_sample_is_not_written_and_retains_failure(self):
        with tempfile.TemporaryDirectory() as root, patch.object(preview_host.observer.HostSampler,'sample',return_value={'payload':'x'*4096}):
            with self.assertRaisesRegex(RuntimeError,'record byte'):
                with preview_host.HostObservation(root,max_bytes=2048,max_record_bytes=1024):pass
            result=json.loads((Path(root)/'host-receipt.json').read_text())
            self.assertFalse(result['complete'])
            self.assertLessEqual((Path(root)/'host.jsonl').stat().st_size,2048)
            self.assertNotIn('x'*1024,(Path(root)/'host.jsonl').read_text())

    def test_host_total_cap_retains_failure_and_never_truncates_records(self):
        with tempfile.TemporaryDirectory() as root, patch.object(preview_host.observer.HostSampler,'sample',return_value={'kind':'sample'}):
            observation=preview_host.HostObservation(root,max_bytes=1024,max_record_bytes=1024)
            observation.__enter__()
            try:
                with self.assertRaisesRegex(RuntimeError,'total byte'):
                    observation.write({'payload':'x'*900})
            finally:result=observation.finish()
            self.assertFalse(result['complete'])
            data=(Path(root)/'host.jsonl').read_bytes()
            self.assertLessEqual(len(data),1024)
            for line in data.splitlines():json.loads(line)

if __name__ == '__main__':
    unittest.main()

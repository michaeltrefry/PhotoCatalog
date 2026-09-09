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


if __name__ == '__main__':
    unittest.main()

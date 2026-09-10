import hashlib
from pathlib import Path
import tempfile
import time
import unittest
import edit_prepare as preparation


class SourceCopyAdmission(unittest.TestCase):
    def test_copy_is_independent_and_preserves_source(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder).resolve();source=root/'original';source.write_bytes(b'unchanged original')
            sha=hashlib.sha256(source.read_bytes()).hexdigest()
            result=preparation.source_copy(source,root/'copy',1024,sha,0,time.monotonic()+30)
            self.assertEqual((root/'copy').read_bytes(),source.read_bytes())
            self.assertEqual(result['sha256'],sha)
            self.assertNotEqual(source.stat().st_ino,(root/'copy').stat().st_ino)

    def test_small_limit_refuses_before_creating_output(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder).resolve();source=root/'original';source.write_bytes(b'123456789')
            with self.assertRaises(ValueError):preparation.source_copy(source,root/'copy',2,'a'*64,0,time.monotonic()+30)
            self.assertFalse((root/'copy').exists())
            self.assertEqual(source.read_bytes(),b'123456789')

    def test_wrong_expected_bytes_preserve_failed_copy_and_source(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder).resolve();source=root/'original';source.write_bytes(b'original')
            with self.assertRaises(ValueError):preparation.source_copy(source,root/'copy',1024,'a'*64,0,time.monotonic()+30)
            self.assertEqual(source.read_bytes(),b'original')
            self.assertEqual((root/'copy').read_bytes(),b'original')

if __name__=='__main__':unittest.main()

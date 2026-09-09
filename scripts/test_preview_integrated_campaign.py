"""Small correctness fixtures only; never opens a database or renders pixels."""
import tempfile
import unittest
from pathlib import Path
from preview_integrated_campaign import raw_copy, source_state, checked_binding


class CopyContract(unittest.TestCase):
    def test_all_companions_and_absence_are_preserved(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp)
            source=root/"original.sqlite3"
            source.write_bytes(b"fixture main bytes")
            Path(str(source)+"-wal").write_bytes(b"fixture WAL bytes")
            expected=source_state(source)
            copied=root/"copy/catalog.sqlite3"
            proof=raw_copy(source,copied,expected)
            self.assertEqual(proof["copied_before_overlay"],expected)
            self.assertEqual(source_state(source),expected)
            self.assertFalse(Path(str(copied)+"-shm").exists())
            with self.assertRaises(FileExistsError):
                raw_copy(source,copied,expected)

    def test_wrong_source_identity_refuses_before_creating_target(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp); source=root/"source"; source.write_bytes(b"first")
            expected=source_state(source); source.write_bytes(b"other")
            target=root/"copy/catalog.sqlite3"
            with self.assertRaisesRegex(ValueError,"frozen"):
                raw_copy(source,target,expected)
            self.assertFalse(target.parent.exists())

    def test_source_change_during_copy_preserves_failed_copy(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp); source=root/"source"; source.write_bytes(b"first")
            expected=source_state(source); target=root/"copy/catalog.sqlite3"
            with self.assertRaisesRegex(ValueError,"changed during"):
                raw_copy(source,target,expected,lambda:Path(str(source)+"-wal").write_bytes(b"new companion"))
            self.assertEqual(target.read_bytes(),b"first")
            self.assertTrue(Path(str(source)+"-wal").exists())

    def test_binding_requires_exact_clean_source_and_operation(self):
        valid={"version":1,"clean":True,"source_revision":"a"*40,"kind":"run","binary_sha256":"b"*64}
        checked_binding(valid,{"binary":"b"*64},"run")
        for bad in ({**valid,"clean":False},{**valid,"kind":"prepare"},{**valid,"source_revision":"main"},{**valid,"binary_sha256":"c"*64}):
            with self.assertRaises(ValueError):
                checked_binding(bad,{"binary":"b"*64},"run")


if __name__=="__main__":
    unittest.main()

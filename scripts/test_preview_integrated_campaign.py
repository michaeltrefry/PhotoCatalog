"""Small correctness fixtures only; never opens a database or renders pixels."""
import tempfile
import unittest
from pathlib import Path
from preview_integrated_campaign import raw_copy, source_state, checked_binding, ancestry_evidence, INDEX_SQL
from preview_experiment import digest
import json
import copy


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
        valid={"version":2,"clean":True,"source_revision":"a"*40,"kind":"run","binary_sha256":"b"*64}
        checked_binding(valid,{"binary":"b"*64},"run")
        for bad in ({**valid,"clean":False},{**valid,"kind":"prepare"},{**valid,"source_revision":"main"},{**valid,"binary_sha256":"c"*64}):
            with self.assertRaises(ValueError):
                checked_binding(bad,{"binary":"b"*64},"run")

    def test_migration_ancestry_preserves_original_bytes_and_rejects_substitution(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp)
            tables=[["assets",10000000],["organization_assets",10000000],["organization_text",10000000]]
            native={"protocol":1,"complete":True,"mode":"migrate_fixture","count":10000000,"engine_version":"3.51.1","schema_before":4,"schema_after":5,"logical_before":"a"*64,"logical_after":"a"*64,"table_counts_before":tables,"table_counts_after":tables,"index_sql":INDEX_SQL}
            native_path=root/"native.json";native_path.write_text(json.dumps(native,indent=3)+"\n")
            proof={**native,"native_receipt_sha256":digest(native_path),"owned_copy_before_sha256":"b"*64,"owned_copy_after_sha256":"c"*64,"observer":{"exit_code":0,"error":None}}
            proof_path=root/"proof.json";proof_path.write_text(json.dumps(proof,indent=1)+"\n")
            source={"files":{"":{"sha256":"c"*64}},"migration_ancestry":{"proof":{"path":str(proof_path),"sha256":digest(proof_path)},"native":{"path":str(native_path),"sha256":digest(native_path)}}}
            payload=ancestry_evidence(source)
            self.assertEqual(payload["proof"],proof_path.read_bytes())
            self.assertEqual(payload["native"],native_path.read_bytes())
            changed=copy.deepcopy(source);changed["files"][""]["sha256"]="d"*64
            with self.assertRaisesRegex(ValueError,"pristine"):
                ancestry_evidence(changed)
            proof["schema_before"]=5;proof_path.write_text(json.dumps(proof))
            changed=copy.deepcopy(source);changed["migration_ancestry"]["proof"]["sha256"]=digest(proof_path)
            with self.assertRaisesRegex(ValueError,"4-to-5"):
                ancestry_evidence(changed)
            with self.assertRaisesRegex(ValueError,"bytes changed"):
                ancestry_evidence(source)


if __name__=="__main__":
    unittest.main()

"""Small correctness fixtures only; never opens a database or renders pixels."""
import tempfile
import unittest
from pathlib import Path
from preview_integrated_campaign import raw_copy, source_state, checked_binding, ancestry_evidence, INDEX_SQL
from schema_campaign_contract import (
    CURRENT_SCHEMA, expected_added_rows, image_initial_rows, schema6_initial_rows,
)
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
        valid={"version":4,"catalog_schema":CURRENT_SCHEMA,"clean":True,"source_revision":"a"*40,"kind":"run","binary_sha256":"b"*64}
        checked_binding(valid,{"binary":"b"*64},"run")
        for bad in ({**valid,"version":3},{**valid,"catalog_schema":6},{**valid,"clean":False},{**valid,"kind":"prepare"},{**valid,"source_revision":"main"},{**valid,"binary_sha256":"c"*64}):
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

    def test_current_schema_direct_migration_preserves_legacy_bytes(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp)
            tables=[["assets",10000000],["organization_assets",10000000],["organization_text",10000000],["storage_bindings",3]]
            native=dict(protocol=1,complete=True,mode="migrate_fixture",count=10000000,engine_version="3.51.1",schema_before=4,schema_after=5,
                        logical_before="a"*64,logical_after="a"*64,table_counts_before=tables,table_counts_after=tables,index_sql=INDEX_SQL)
            def write(name,value):
                path=root/name;path.write_text(json.dumps(value));return {"path":str(path),"sha256":digest(path)}
            old_native=write("old-native.json",native)
            proof={**native,"native_receipt_sha256":old_native["sha256"],"owned_copy_before_sha256":"b"*64,"owned_copy_after_sha256":"c"*64,"observer":{"exit_code":0,"error":None}}
            old_proof=write("old-proof.json",proof)
            current={**native,"protocol":2,"catalog_schema":CURRENT_SCHEMA,"schema_before":5,"schema_after":CURRENT_SCHEMA,
                     "identity_scope":"pre_existing_tables","added_tables":expected_added_rows(tables,5),
                     "alias_initial_state":{"unbound":9999997,"dirty":3},"image_initial_state":image_initial_rows(tables),
                     "original_columns_preserved":True}
            current_native=write("new-native.json",current)
            upgraded={**current,"native_receipt_sha256":current_native["sha256"],"owned_copy_before_sha256":"c"*64,"owned_copy_after_sha256":"d"*64,"observer":{"exit_code":0,"error":None}}
            current_proof=write("new-proof.json",upgraded)
            source={"schema_version":CURRENT_SCHEMA,"files":{"":{"sha256":"d"*64}},
                    "migration_ancestry":{"proof":old_proof,"native":old_native},
                    "schema13_migration":{"proof":current_proof,"native":current_native}}
            result=ancestry_evidence(source)
            self.assertEqual(set(result),{"proof","native","schema13_proof","schema13_native"})
            self.assertEqual(json.loads(result["native"])["schema_after"],5)
            self.assertEqual(json.loads(result["schema13_native"])["schema_after"],CURRENT_SCHEMA)
            for field,value in [("schema_after",6),("identity_scope","all_tables"),("added_tables",[]),
                                ("alias_initial_state",{"unbound":10000000,"dirty":0}),
                                ("image_initial_state",[]),("original_columns_preserved",False),
                                ("logical_after","f"*64),("owned_copy_before_sha256","e"*64),
                                ("owned_copy_after_sha256","c"*64)]:
                bad=copy.deepcopy(source)
                bad["schema13_migration"]["proof"]=write("bad-proof.json",{**upgraded,field:value})
                with self.subTest(field=field),self.assertRaises(ValueError):
                    ancestry_evidence(bad)
            self.assertEqual(ancestry_evidence(source)["native"],result["native"])
            current_native_path=Path(current_native["path"]);current_native_bytes=current_native_path.read_bytes()
            current_native_path.write_bytes(current_native_bytes+b"substituted")
            with self.assertRaisesRegex(ValueError,"bytes changed"):
                ancestry_evidence(source)
            current_native_path.write_bytes(current_native_bytes)

    def test_current_schema_accepts_only_a_truthful_five_six_thirteen_chain(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp)
            tables=[["assets",10000000],["organization_assets",10000000],["organization_text",10000000],["storage_bindings",3]]
            legacy=dict(protocol=1,complete=True,mode="migrate_fixture",count=10000000,engine_version="3.51.1",
                        schema_before=4,schema_after=5,logical_before="a"*64,logical_after="a"*64,
                        table_counts_before=tables,table_counts_after=tables,index_sql=INDEX_SQL)
            def write(name,value):
                path=root/name;path.write_text(json.dumps(value));return {"path":str(path),"sha256":digest(path)}
            legacy_native=write("legacy-native.json",legacy)
            legacy_proof=write("legacy-proof.json",{**legacy,"native_receipt_sha256":legacy_native["sha256"],
                "owned_copy_before_sha256":"b"*64,"owned_copy_after_sha256":"c"*64,"observer":{"exit_code":0,"error":None}})
            six={**legacy,"protocol":2,"catalog_schema":6,"schema_before":5,"schema_after":6,
                 "identity_scope":"pre_existing_tables","added_tables":schema6_initial_rows(tables),
                 "alias_initial_state":{"unbound":9999997,"dirty":3}}
            six_native=write("six-native.json",six)
            six_proof=write("six-proof.json",{**six,"native_receipt_sha256":six_native["sha256"],
                "owned_copy_before_sha256":"c"*64,"owned_copy_after_sha256":"d"*64,"observer":{"exit_code":0,"error":None}})
            six_tables=sorted(tables+six["added_tables"])
            thirteen={**six,"catalog_schema":CURRENT_SCHEMA,"schema_before":6,"schema_after":CURRENT_SCHEMA,
                      "table_counts_before":six_tables,"table_counts_after":six_tables,
                      "added_tables":expected_added_rows(six_tables,6),
                      "image_initial_state":image_initial_rows(six_tables),"original_columns_preserved":True}
            thirteen_native=write("thirteen-native.json",thirteen)
            thirteen_proof=write("thirteen-proof.json",{**thirteen,"native_receipt_sha256":thirteen_native["sha256"],
                "owned_copy_before_sha256":"d"*64,"owned_copy_after_sha256":"e"*64,"observer":{"exit_code":0,"error":None}})
            source={"schema_version":CURRENT_SCHEMA,"files":{"":{"sha256":"e"*64}},
                    "migration_ancestry":{"proof":legacy_proof,"native":legacy_native},
                    "schema6_migration":{"proof":six_proof,"native":six_native},
                    "schema13_migration":{"proof":thirteen_proof,"native":thirteen_native}}
            result=ancestry_evidence(source)
            self.assertEqual(set(result),{"proof","native","schema6_proof","schema6_native","schema13_proof","schema13_native"})
            for section,field,value in (("schema6_migration","owned_copy_before_sha256","f"*64),
                                        ("schema13_migration","owned_copy_before_sha256","c"*64),
                                        ("schema13_migration","table_counts_before",tables)):
                bad=copy.deepcopy(source)
                original=json.loads(Path(bad[section]["proof"]["path"]).read_text())
                bad[section]["proof"]=write("bad-"+section+"-proof.json",{**original,field:value})
                with self.subTest(section=section,field=field),self.assertRaises(ValueError):
                    ancestry_evidence(bad)


if __name__=="__main__":
    unittest.main()

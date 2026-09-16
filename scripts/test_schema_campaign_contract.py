import copy
import unittest

from schema_campaign_contract import (
    CURRENT_SCHEMA, INDEX_SQL, SCHEMA6_TABLES, SCHEMA7_TABLES,
    TABLES_BY_SCHEMA, alias_initial_state, expected_added_rows,
    image_initial_rows, schema6_initial_rows, validate_migration_receipt,
)


def base(count=1000, bound=7):
    return [["assets", count], ["organization_assets", count],
            ["organization_text", count], ["storage_bindings", bound]]


def source_tables(schema, count=1000, bound=7):
    tables=base(count,bound)
    for version,names in TABLES_BY_SCHEMA:
        if version<=schema:
            rows=schema6_initial_rows(tables) if version==6 else (
                image_initial_rows(tables) if version==7 else
                [[name,1 if name=="organization_collection_zero_backfill" else 0]
                 for name in sorted(names)])
            tables=sorted(tables+rows)
    return tables


def receipt(source, count=1000, bound=7):
    tables=source_tables(source,count,bound)
    return {"protocol":2,"catalog_schema":CURRENT_SCHEMA,"complete":True,
            "mode":"migrate_fixture","count":count,"schema_before":source,
            "schema_after":CURRENT_SCHEMA,"identity_scope":"pre_existing_tables",
            "logical_before":"a"*64,"logical_after":"a"*64,
            "table_counts_before":tables,"table_counts_after":tables,
            "added_tables":expected_added_rows(tables,source),
            "alias_initial_state":alias_initial_state(tables),
            "image_initial_state":image_initial_rows(tables),
            "original_columns_preserved":True,"index_sql":INDEX_SQL}


class SchemaCampaignContractTests(unittest.TestCase):
    def test_exact_cumulative_additions_and_initial_rows(self):
        self.assertEqual(len(schema6_initial_rows(base())),13)
        self.assertEqual(len(SCHEMA7_TABLES),32)
        self.assertEqual(len(expected_added_rows(base(),4)),57)
        self.assertEqual(len(expected_added_rows(base(),5)),57)
        self.assertEqual(len(expected_added_rows(source_tables(6),6)),44)
        self.assertEqual(expected_added_rows(source_tables(13),13),[])
        rows=dict(expected_added_rows(source_tables(6),6))
        self.assertEqual(rows["catalog_images"],1000)
        self.assertEqual(rows["image_shared_state"],1000)
        self.assertEqual(rows["migration_mapping_epoch"],1)
        self.assertEqual(rows["organization_collection_zero_backfill"],1)
        self.assertEqual(rows["metadata_write_receipts"],0)
        alias=dict(schema6_initial_rows(base(bound=7)))
        self.assertEqual(alias["export_alias_state"],1)
        self.assertEqual(alias["export_alias_dirty"],7)

    def test_current_receipts_accept_four_five_six_and_noop_thirteen(self):
        for source in (4,5,6,13):
            with self.subTest(source=source):
                value=receipt(source)
                self.assertEqual(validate_migration_receipt(value,1000,source,13),
                                 value["table_counts_before"])

    def test_current_receipts_reject_wrong_proof_surfaces(self):
        valid=receipt(6)
        changes=(
            lambda r:r.update(catalog_schema=6),
            lambda r:r.update(schema_after=12),
            lambda r:r.update(logical_after="b"*64),
            lambda r:r.update(added_tables=[]),
            lambda r:r.update(alias_initial_state={"unbound":1000,"dirty":0}),
            lambda r:r.update(image_initial_state=[]),
            lambda r:r.update(original_columns_preserved=False),
            lambda r:r.update(index_sql="CREATE INDEX wrong ON assets(id)"),
        )
        for change in changes:
            bad=copy.deepcopy(valid);change(bad)
            with self.assertRaises(ValueError):
                validate_migration_receipt(bad,1000,6,13)
        bad=receipt(6)
        bad["table_counts_before"]=[row for row in bad["table_counts_before"] if row[0]!="edit_changes"]
        bad["table_counts_after"]=bad["table_counts_before"]
        with self.assertRaises(ValueError):
            validate_migration_receipt(bad,1000,6,13)
        bad=receipt(6)
        bad["table_counts_before"][0][1]=999
        bad["table_counts_after"]=copy.deepcopy(bad["table_counts_before"])
        with self.assertRaises(ValueError):
            validate_migration_receipt(bad,1000,6,13)

    def test_schema_six_historical_contract_stays_frozen(self):
        tables=base()
        historical={"protocol":2,"catalog_schema":6,"complete":True,
            "mode":"migrate_fixture","count":1000,"schema_before":5,"schema_after":6,
            "identity_scope":"pre_existing_tables","logical_before":"a"*64,
            "logical_after":"a"*64,"table_counts_before":tables,"table_counts_after":tables,
            "added_tables":schema6_initial_rows(tables),
            "alias_initial_state":alias_initial_state(tables),"index_sql":INDEX_SQL}
        validate_migration_receipt(historical,1000,5,6)
        for field,value in (("catalog_schema",13),("schema_after",13),("added_tables",[])):
            bad=copy.deepcopy(historical);bad[field]=value
            with self.assertRaises(ValueError):
                validate_migration_receipt(bad,1000,5,6)


if __name__=="__main__":
    unittest.main()

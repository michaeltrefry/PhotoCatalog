"""Shared, deterministic catalog-migration receipt contracts for campaigns."""

CURRENT_SCHEMA = 13
NATIVE_PROTOCOL = 2
INDEX_SQL = "CREATE INDEX organization_lens_capture ON organization_assets(lens,capture,sequence)"

SCHEMA6_TABLES = (
    "edit_changes", "edit_copy_items", "edit_copy_jobs", "edit_recipe_nodes",
    "edit_redo_nodes", "edit_variants", "export_alias_directories",
    "export_alias_dirty", "export_alias_paths", "export_alias_state",
    "photo_export_blobs", "photo_export_items", "photo_export_jobs",
)
SCHEMA7_TABLES = (
    "catalog_images", "image_import_map", "image_import_reservations",
    "image_shared_events", "image_shared_state", "image_storage_events",
    "metadata_image_export_authorities", "metadata_image_observations",
    "metadata_image_sources", "migration_artifacts", "migration_images",
    "migration_record_lookup", "migration_lookup_backfill",
    "migration_file_metadata", "migration_runs", "migration_run_supplements",
    "migration_run_items", "migration_reconciliation", "migration_mapping_epoch",
    "migration_metadata", "migration_organization", "migration_evidence",
    "migration_evidence_blobs", "migration_evidence_chunks", "migration_originals",
    "migration_retained_fields", "migration_retained_records", "migration_retention",
    "organization_collection_order", "organization_collection_structure",
    "organization_image_relations", "organization_keyword_synonyms",
)
SCHEMA8_TABLES = (
    "migration_current_repairs", "migration_current_repair_items",
    "migration_current_repair_reports",
)
SCHEMA10_TABLES = (
    "migration_keyword_repairs", "migration_keyword_repair_items",
    "migration_keyword_repair_reports",
)
SCHEMA11_TABLES = (
    "organization_collection_zero", "organization_collection_zero_backfill",
)
SCHEMA12_TABLES = (
    "storage_review_summary", "storage_source_fences", "storage_hydration_transitions",
)
SCHEMA13_TABLES = ("metadata_write_receipts",)

TABLES_BY_SCHEMA = (
    (6, SCHEMA6_TABLES), (7, SCHEMA7_TABLES), (8, SCHEMA8_TABLES),
    (10, SCHEMA10_TABLES), (11, SCHEMA11_TABLES),
    (12, SCHEMA12_TABLES), (13, SCHEMA13_TABLES),
)


def table_counts(tables):
    if (not isinstance(tables, list) or len(dict(tables)) != len(tables)
            or any(not isinstance(name, str) or type(count) is not int or count < 0
                   for name, count in tables)):
        raise ValueError("invalid migration table counts")
    counts = dict(tables)
    assets, bound = counts.get("assets"), counts.get("storage_bindings")
    if type(assets) is not int or type(bound) is not int or not 0 <= bound <= assets:
        raise ValueError("binding cardinality required for migration initialization")
    return counts


def alias_initial_state(tables):
    counts = table_counts(tables)
    return {"unbound": counts["assets"] - counts["storage_bindings"],
            "dirty": counts["storage_bindings"]}


def initial_row_count(name, counts):
    if name == "export_alias_state":
        return 1
    if name == "export_alias_dirty":
        return counts["storage_bindings"]
    if name in ("catalog_images", "image_shared_state"):
        return counts["assets"]
    if name in ("migration_mapping_epoch", "organization_collection_zero_backfill"):
        return 1
    return 0


def initial_rows_for(tables, names):
    counts = table_counts(tables)
    return [[name, initial_row_count(name, counts)] for name in sorted(names)]


def schema6_initial_rows(tables):
    """Frozen 5-to-6 addition contract."""
    return initial_rows_for(tables, SCHEMA6_TABLES)


def image_initial_rows(tables):
    return initial_rows_for(tables, SCHEMA7_TABLES)


def expected_added_rows(tables, source_schema, target_schema=CURRENT_SCHEMA):
    if type(source_schema) is not int or not 4 <= source_schema <= target_schema <= CURRENT_SCHEMA:
        raise ValueError("unsupported migration schema range")
    names = [name for version, group in TABLES_BY_SCHEMA
             if source_schema < version <= target_schema for name in group]
    return initial_rows_for(tables, names)


def validate_table_roster(tables, source_schema):
    """Check table presence and pristine rows known for a source fixture."""
    counts = table_counts(tables)
    for version, names in TABLES_BY_SCHEMA:
        present = [name in counts for name in names]
        if any(present) != all(present) or all(present) != (source_schema >= version):
            raise ValueError(f"schema{version} table roster disagrees with source version")
    if source_schema >= 6:
        for name, expected in schema6_initial_rows(tables):
            if counts[name] != expected:
                raise ValueError("schema6 fixture has non-initial edit/export/alias state")
    if source_schema >= 7:
        for name, expected in image_initial_rows(tables):
            if counts[name] != expected:
                raise ValueError("schema7 fixture has non-initial image/import state")
    for version, names in TABLES_BY_SCHEMA:
        if source_schema >= version and version in (8, 10, 12, 13):
            if any(counts[name] != 0 for name in names):
                raise ValueError("fixture has unexpected repair, review or receipt state")
    return counts


def validate_migration_receipt(native, count, source_schema, target_schema):
    """Validate native migration output without reinterpreting older protocols."""
    if native.get("protocol") != NATIVE_PROTOCOL or native.get("catalog_schema") != target_schema:
        raise ValueError("native migration protocol/schema differs")
    if (native.get("complete") is not True or native.get("mode") != "migrate_fixture"
            or native.get("count") != count or native.get("schema_before") != source_schema
            or native.get("schema_after") != target_schema
            or native.get("identity_scope") != "pre_existing_tables"):
        raise ValueError("native migration identity differs")
    logical = native.get("logical_before")
    if (not isinstance(logical, str) or len(logical) != 64
            or any(c not in "0123456789abcdef" for c in logical)
            or native.get("logical_after") != logical):
        raise ValueError("migration logical identity changed")
    tables = native.get("table_counts_before")
    if tables != native.get("table_counts_after"):
        raise ValueError("migration table identity changed")
    counts = validate_table_roster(tables, source_schema)
    if any(counts.get(name) != count for name in ("assets", "organization_assets", "organization_text")):
        raise ValueError("fixture cardinality differs")
    if native.get("added_tables") != expected_added_rows(tables, source_schema, target_schema):
        raise ValueError("migration added-table identity differs")
    if native.get("alias_initial_state") != alias_initial_state(tables):
        raise ValueError("migration alias initialization differs")
    if native.get("index_sql") != INDEX_SQL:
        raise ValueError("migration index differs")
    if target_schema >= 7:
        if native.get("image_initial_state") != image_initial_rows(tables):
            raise ValueError("migration image initialization differs")
        if native.get("original_columns_preserved") is not True:
            raise ValueError("migration original columns were not preserved")
    return tables


def proof_fields(target_schema):
    fields = ["schema_before", "schema_after", "logical_before", "logical_after", "table_counts_before",
              "table_counts_after", "identity_scope", "added_tables",
              "alias_initial_state", "index_sql"]
    if target_schema >= 7:
        fields += ["image_initial_state", "original_columns_preserved"]
    return tuple(fields)

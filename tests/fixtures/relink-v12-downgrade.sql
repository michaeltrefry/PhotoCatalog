-- Synthetic downgrade fixtures only: restore the exact schema11 storage shape.
-- Call before removing older schemas or lowering user_version.
DROP TABLE metadata_write_receipts;
DROP TRIGGER storage_review_plan;
DROP TRIGGER storage_review_item_insert;
DROP TRIGGER storage_review_item_update;
DROP TRIGGER storage_review_source_insert;
DROP TRIGGER storage_review_source_update;
DROP TRIGGER storage_reviewed_fingerprint;
DROP TABLE storage_hydration_transitions;
DROP TABLE storage_source_fences;
DROP TABLE storage_review_summary;
ALTER TABLE storage_plans DROP COLUMN revision;
ALTER TABLE storage_plans DROP COLUMN review_token;
ALTER TABLE storage_plans DROP COLUMN rules;

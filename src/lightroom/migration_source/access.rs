//! Complete read interface shared by the local CLI source and owned desktop
//! reader proxy. Projection code consumes this interface without acquiring a
//! source pathname or constructing an inspection connection.
use super::*;
use crate::lightroom::capture::Manifest;

pub(crate) trait MigrationRead {
    /// Resource admission for destination retention, before copying/parsing a
    /// borrowed saved cursor. Legacy CLI readers keep their existing policy;
    /// managed readers charge their parent-owned operation pool. This is local
    /// coordination, not a new Source query or a data-format byte limit.
    fn admit_retention(&self, _cursor_bytes: usize) -> Result<()> {
        Ok(())
    }

    fn seal(&self) -> &InputSeal;
    fn binding_blake3(&self) -> &str;
    fn max_chunk_bytes(&self) -> usize;
    fn capture_manifest(&self, revision: &str) -> Result<Manifest>;
    fn stable_source(&self, revision: &str, source_id: &str) -> Result<StableSource>;
    fn origin_packet_roster(
        &self,
        revision: &str,
        source_id: &str,
        origin: &str,
    ) -> Result<Vec<i64>>;
    fn page(
        &self,
        revision: &str,
        collection: Collection,
        after: Option<&Cursor>,
        limit: usize,
    ) -> Result<Page>;
    fn read_chunk(&self, reference: &ByteRef, offset: u64, limit: usize) -> Result<Vec<u8>>;
    fn count(&self, revision: &str, collection: Collection) -> Result<u64>;
    fn resolve(
        &self,
        revision: &str,
        source_id: &str,
        field: &str,
        target_table: &str,
    ) -> Result<Resolution>;
    fn image_links(&self, revision: &str, source_id: &str) -> Result<ImageLinks>;
}
impl MigrationRead for MigrationSource {
    fn seal(&self) -> &InputSeal {
        MigrationSource::seal(self)
    }
    fn binding_blake3(&self) -> &str {
        MigrationSource::binding_blake3(self)
    }
    fn max_chunk_bytes(&self) -> usize {
        MigrationSource::max_chunk_bytes(self)
    }
    fn capture_manifest(&self, revision: &str) -> Result<Manifest> {
        MigrationSource::capture_manifest(self, revision)
    }
    fn stable_source(&self, revision: &str, source_id: &str) -> Result<StableSource> {
        MigrationSource::stable_source(self, revision, source_id)
    }
    fn origin_packet_roster(
        &self,
        revision: &str,
        source_id: &str,
        origin: &str,
    ) -> Result<Vec<i64>> {
        MigrationSource::origin_packet_roster(self, revision, source_id, origin)
    }
    fn page(
        &self,
        revision: &str,
        collection: Collection,
        after: Option<&Cursor>,
        limit: usize,
    ) -> Result<Page> {
        MigrationSource::page(self, revision, collection, after, limit)
    }
    fn read_chunk(&self, reference: &ByteRef, offset: u64, limit: usize) -> Result<Vec<u8>> {
        MigrationSource::read_chunk(self, reference, offset, limit)
    }
    fn count(&self, revision: &str, collection: Collection) -> Result<u64> {
        MigrationSource::count(self, revision, collection)
    }
    fn resolve(
        &self,
        revision: &str,
        source_id: &str,
        field: &str,
        target_table: &str,
    ) -> Result<Resolution> {
        MigrationSource::resolve(self, revision, source_id, field, target_table)
    }
    fn image_links(&self, revision: &str, source_id: &str) -> Result<ImageLinks> {
        MigrationSource::image_links(self, revision, source_id)
    }
}

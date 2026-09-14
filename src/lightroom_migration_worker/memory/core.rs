//! Core-owned phase graphs. These are additional to Source/relay owners and
//! depend on the existing retained-document limits, never a new format ceiling.
use super::layout::{
    add, content_containers, manifest_dynamic, mul, saved_policy_dynamic, seal_dynamic,
    seal_validation, tree,
};
use crate::{
    catalog_migration::artifacts::ArtifactRequest,
    lightroom::{
        MANIFEST_BYTES,
        migration_source::{Collection, Field},
        plan::Cell,
    },
    lightroom_migration_worker::protocol::FRAME_BYTES,
    storage_volume::NativePath,
};
use anyhow::Result;
use std::mem::size_of;

const RETAINED_BYTES: usize = 8 * 1024 * 1024;
const DESCRIPTOR_BYTES: usize = 64 * 1024;

/// Caller-owned state persists after importer::read returns. The fifth Progress
/// is Worker's outer `before`; the other four cover nested reconciliation.
fn saved_state_retained() -> Result<usize> {
    add(
        mul(2, saved_policy_dynamic(RETAINED_BYTES)?)?,
        mul(5, RETAINED_BYTES)?,
    )
}

/// Raw saved Policy is parsed directly: its internally tagged overlap enum or
/// one NativePath units-before-tag can buffer Content, but neither contains the
/// other. One container vocabulary layer is live, not one per Artifact entry.
/// The extra Policy graph allows current item and Vec reallocation overlap.
pub(crate) fn saved_state() -> Result<usize> {
    let working = add(
        saved_policy_dynamic(RETAINED_BYTES)?,
        add(
            // Two raw documents plus new <=2R / old <=R hash-encoder backing.
            mul(5, RETAINED_BYTES)?,
            add(
                content_containers(RETAINED_BYTES, 1)?,
                add(mul(32, RETAINED_BYTES)?, mul(8, FRAME_BYTES)?)?,
            )?,
        )?,
    )?;
    add(saved_state_retained()?, working)
}

/// The Artifact-only record projection admits exactly Captures' seven fields
/// and one key. Every decoded hex payload, including a duplicate replacement,
/// consumes two original bytes. ByteRef identity/name copies are fixed by the
/// admitted roster, including one old/new duplicate replacement and the borrowed
/// head revision's possible unescape buffer. Parser scratch and raw are separate.
fn capture_record() -> Result<usize> {
    let (keys, fields, _) = Collection::Captures.transport_shape();
    let names = fields
        .iter()
        .try_fold(0usize, |n, field| add(n, field.len()))?;
    let longest = fields.iter().map(|name| name.len()).max().unwrap_or(0);
    let payload = add(
        RETAINED_BYTES / 2,
        add(
            mul(add(fields.len(), 1)?, 128)?,
            add(add(mul(2, names)?, mul(2, longest)?)?, 128)?,
        )?,
    )?;
    add(
        payload,
        add(
            mul(keys.len(), size_of::<Cell>())?,
            tree::<String, Field>(fields.len())?,
        )?,
    )
}

fn path_clone(path: &NativePath) -> Result<usize> {
    match path {
        NativePath::UnixBytes(value) => Ok(value.len()),
        NativePath::WindowsWide(value) => mul(value.len(), size_of::<u16>()),
    }
}

/// Whole ArtifactFactory caller phase, admitted before destination descriptor
/// lookup. The original request is already caller-owned; only its actual clone
/// lengths are added here, so no generated-Policy path/identity shortcut is used.
/// Manifest and seal roots stay in their real stack owners, not invented Boxes.
/// Compression/SQLite/runtime workspace is distinct from these payload graphs.
pub(crate) fn artifact_constructor(request: &ArtifactRequest) -> Result<usize> {
    let record = capture_record()?;
    // selected_capture: raw seal R, compressed R+32768 and growing decoded raw
    // <=2*(R+1)+8 plus its old <=R+1 backing at realloc, seal graph/
    // validation and its canonical M-byte encoder. The raw seal binding stays live.
    let opening = add(
        add(mul(5, RETAINED_BYTES)?, 32779)?,
        add(
            add(seal_dynamic()?, seal_validation()?)?,
            mul(3, MANIFEST_BYTES)?,
        )?,
    )?;
    // field_bytes: new complete Manifest backing <=2M plus old <=M at realloc.
    // Five chunk lengths cover compressed, growing decoded and old decoded
    // backing. The bounded descriptor raw/typed strings have their own 3C;
    // parser/error scratch is charged in the larger parser term below.
    let field_bytes = add(
        mul(3, MANIFEST_BYTES)?,
        add(
            mul(5, crate::catalog_migration::evidence::CHUNK_BYTES)?,
            add(mul(3, DESCRIPTOR_BYTES)?, 32768 + 4096)?,
        )?,
    )?;
    // Artifact clone payload is a subset of Manifest's exact owned strings and
    // native units. Its upper M accounts the complete original encoded document.
    let clone = add(
        add(MANIFEST_BYTES, 1)?,
        add(
            add(
                path_clone(&request.mapping.root)?,
                path_clone(&request.mapping.relative)?,
            )?,
            add(
                request.mapping.copy_identity.object.len(),
                request.mapping.copy_identity.changed.len(),
            )?,
        )?,
    )?;
    let manifest_validation = add(
        field_bytes,
        add(manifest_dynamic()?, mul(3, MANIFEST_BYTES)?)?,
    )?;
    let final_construction = add(
        add(field_bytes, manifest_dynamic()?)?,
        add(
            add(MANIFEST_BYTES, add(seal_dynamic()?, seal_validation()?)?)?,
            add(clone, mul(3, DESCRIPTOR_BYTES)?)?,
        )?,
    )?;
    // RawValue decoders have no complete Content graph. This covers their raw,
    // scalar scratch and bounded custom-error conversion separately from the
    // partial/final typed graphs above, at the larger Manifest byte limit.
    let parser = add(mul(32, MANIFEST_BYTES)?, mul(8, FRAME_BYTES)?)?;
    Ok(saved_state()?.max(add(
        saved_state_retained()?,
        add(
            record,
            add(
                opening.max(manifest_validation).max(final_construction),
                parser,
            )?,
        )?,
    )?))
}

/// lookup::authority accepts direct public InputSeal serde, then only checks
/// its bounded canonical hash and revision membership. It does not validate
/// Source roster caps. Preserve that saved-state grammar when charging this
/// nested phase; a roster-capped Source decoder is not interchangeable here.
fn lookup_authority() -> Result<usize> {
    use crate::lightroom::migration_source::{SelectedCapture, SupplementPin};
    let selected = size_of::<SelectedCapture>();
    let pin = size_of::<SupplementPin>();
    let string = size_of::<String>();
    // Five/six required struct values need at least 11/13 sequence bytes even
    // pretending every value is one byte; maps cannot be shorter. A String
    // needs two. Separators imply 12a+14s+3e <=R+3 (three missing last commas).
    // Three element sizes include retained new <=2n and old <=n Vec backing.
    // Round each ratio upward, preserving a checked target-layout upper bound.
    let coefficient = mul(3, selected)?
        .div_ceil(12)
        .max(mul(3, pin)?.div_ceil(14))
        .max(mul(3, string)?.div_ceil(3));
    let vectors = add(
        mul(add(RETAINED_BYTES, 3)?, coefficient)?,
        // Each vector's eight-slot growth minimum and one current typed member.
        mul(9, add(add(selected, pin)?, string)?)?,
    )?;
    // All strings and the one NativePath unit vector share original R bytes.
    // 5R+32 covers string capacities plus new/old native-unit backing/minima.
    let graph = add(vectors, add(mul(5, RETAINED_BYTES)?, 32)?)?;
    // BLOB remains SQLite-owned. Keep the complete conservative raw/error family
    // anyway; Content is only the one adjacent NativePath units value. The
    // canonical encoder can retain new <=2M and old <=M backing while growing.
    add(
        graph,
        add(
            content_containers(RETAINED_BYTES, 1)?,
            add(
                mul(3, MANIFEST_BYTES)?,
                add(mul(32, RETAINED_BYTES)?, mul(8, FRAME_BYTES)?)?,
            )?,
        )?,
    )
}

/// Full destination retention phase, excluding Source-owned replies already
/// admitted by Client. A saved cursor has no inherited raw format ceiling: use
/// its borrowed SQLite byte length before copying or parsing it. The same floor
/// covers next_cursor (which only needs a String) without narrowing its grammar.
pub(crate) fn retention(cursor_bytes: usize) -> Result<usize> {
    use super::layout::{record_dynamic, vector};
    use crate::{
        catalog_migration::lookup::PreparedIndex, lightroom::migration_source::EvidenceRecord,
    };
    // A pending record decoder borrows compressed SQLite storage. Its growing
    // raw Vec, one generic Cell vocabulary, typed record and parser/error owners
    // can coexist. Field trees already include their split cascade; only the
    // key Vec needs a second backing allowance during reallocation. A later
    // pending_field clones one ByteRef, whose payload is a subset of the raw R.
    // Charge that R explicitly even though decoding scratch has then retired.
    let pending = add(
        add(
            record_dynamic(RETAINED_BYTES, 1)?,
            vector::<Cell>(RETAINED_BYTES / 2)?,
        )?,
        add(
            content_containers(RETAINED_BYTES, 1)?,
            add(mul(37, RETAINED_BYTES)?, mul(8, FRAME_BYTES)?)?,
        )?,
    )?;
    // Source's canonical Page ceiling bounds the sum of staged canonical bytes.
    // Keep a next-index temporary conservatively even for a defensive local
    // MigrationRead that exceeds the remote Page ceiling. Up to four copied
    // 4096-byte keys occur per record. The remaining metadata allowance comes
    // from the actual fixed UnavailableKey and Cursor serializer vocabulary.
    const FIELD_NAME: usize = "target_table".len();
    const REASON: usize = "UnsupportedCollection".len();
    const UNAVAILABLE: usize =
        2 + 4 * (r#"{"field":"","reason":""}"#.len() + FIELD_NAME + REASON + 1);
    const CURSOR_FRAMING: usize =
        r#"{"seal":"","revision":"","collection":"MetadataFacts","after":[]}"#.len() + 128;
    let metadata = 2 * UNAVAILABLE + 3 * 64 + 2 * CURSOR_FRAMING;
    // 5R canonical current/queued/realloc, 3R compressed, 3R cursor strings,
    // R current hex scratch and 2R current key/reference payload copies.
    let staged_buffers = add(
        mul(14, RETAINED_BYTES)?,
        add(mul(3 * 64, 32768)?, mul(65, 4 * 4096 + metadata)?)?,
    )?;
    let staged_buffers = add(
        staged_buffers,
        add(
            vector::<crate::catalog_migration::lookup::UnavailableKey>(4)?,
            add(4 * FIELD_NAME, vector::<Cell>(3)?)?,
        )?,
    )?;
    let staged = add(
        add(staged_buffers, lookup_authority()?)?,
        vector::<(
            &EvidenceRecord,
            Vec<u8>,
            usize,
            String,
            String,
            bool,
            PreparedIndex,
        )>(64)?,
    )?;
    // A Cursor owns strings and a Vec<Cell>; malformed/partial values still use
    // their original byte grammar. Two vector envelopes cover old/new backing
    // at realloc. Content and errors are separate, not another retained cursor.
    let cursor = add(
        mul(2, vector::<Cell>(cursor_bytes)?)?,
        add(
            mul(5, cursor_bytes)?,
            add(
                content_containers(cursor_bytes, 1)?,
                add(mul(32, cursor_bytes)?, mul(8, FRAME_BYTES)?)?,
            )?,
        )?,
    )?;
    Ok(saved_state()?.max(add(
        saved_state_retained()?,
        add(pending.max(staged), cursor)?,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retention_phase_admits_actual_cursor_length_without_a_raw_format_ceiling() -> Result<()> {
        let authority = lookup_authority()?;
        let empty = retention(0)?;
        assert!(empty > authority);

        let at = retention(RETAINED_BYTES)?;
        let larger = retention(RETAINED_BYTES + 1)?;
        assert!(at > empty && larger > at);
        assert!(retention(usize::MAX).is_err());
        println!(
            "CORE_RETENTION_EXPRESSION empty_cursor={empty} nested_generic_seal={authority} cursor_at_old_record_bytes={at} cursor_one_more_byte={larger} no_cursor_format_cap=true"
        );
        Ok(())
    }
    #[test]
    fn artifact_clone_terms_use_native_units_and_actual_unrestricted_identity_text() -> Result<()> {
        assert_eq!(path_clone(&NativePath::UnixBytes(vec![0, 128, 255]))?, 3);
        assert_eq!(
            path_clone(&NativePath::WindowsWide(vec![0, 0xd800, 0xffff]))?,
            6
        );
        let request = ArtifactRequest {
            retained_capture_record: 1,
            member_index: 0,
            mapping: crate::catalog_migration::artifacts::ArtifactMapping {
                root: NativePath::UnixBytes(vec![b'/']),
                relative: NativePath::UnixBytes(vec![b'a']),
                copy_identity: crate::lightroom::migration_source::FileIdentity {
                    object: "é".repeat(500),
                    changed: "x".repeat(600),
                    bytes: 1,
                    modified_ns: Some(u128::MAX),
                },
            },
        };
        let cost = artifact_constructor(&request)?;
        assert!(cost >= saved_state()?);
        assert!(capture_record()? >= RETAINED_BYTES / 2);
        println!(
            "CORE_ARTIFACT_EXPRESSION bytes={cost} identity_object_bytes=1000 identity_changed_bytes=600 native_layout=true"
        );
        Ok(())
    }
}

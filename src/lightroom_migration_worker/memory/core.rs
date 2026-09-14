//! Core-owned phase graphs. These are additional to Source/relay owners and
//! depend on the existing retained-document limits, never a new format ceiling.
use super::layout::{
    add, content_containers, manifest_dynamic, mul, record_dynamic, saved_policy_dynamic,
    seal_dynamic, seal_validation, tree, tree_layout, vector, vector_layout,
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
const FILE_METADATA_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;
// reader::origin_packet_roster reads LIMIT 2049 and rejects more than 2048;
// file_metadata accepts the same existing 1024 packet + 1024 parse-input set.
const FILE_METADATA_PACKET_RECORDS: usize = 2048;
// lookup::page checks the stored classification before parsing each hit.
const LOOKUP_CLASSIFICATION_BYTES: usize = 4096;

/// Complete direct-serde phase for one already admitted JSON document. The
/// `Content` term covers buffered internally/externally tagged containers and
/// `Value`; 32 bytes per source byte covers typed Vec/map/string roots and
/// their pinned growth overlap (the same established parser family used by
/// selected metadata and Artifact construction). Eight maximum wire frames
/// cover serde error/path and current chunk owners without borrowing another
/// document's allowance. Callers add raw input and named retained type graphs.
fn worker_json(bytes: usize, content_layers: usize) -> Result<usize> {
    add(
        content_containers(bytes, content_layers)?,
        add(mul(32, bytes)?, mul(8, FRAME_BYTES)?)?,
    )
}

/// The small operation envelope is decoded before LM can receive grants. Its
/// parent therefore admits this complete typed/direct-serde graph before spawn;
/// the existing separate INPUT_BYTES reservation retains the raw child String.
pub(crate) fn worker_envelope(bytes: usize) -> Result<usize> {
    worker_json(bytes, 2)
}

/// Run keeps its parsed seal and Policy, temporarily owns both approval Value
/// and ApprovalDocument, and clones the seal once while the original remains.
/// Raw multipart Strings are separately admitted by `input` and remain live.
pub(crate) fn worker_run_documents(
    seal_bytes: usize,
    approval_bytes: usize,
    policy_bytes: usize,
    authorization_bytes: usize,
) -> Result<usize> {
    let seal = seal_dynamic()?;
    let policy = saved_policy_dynamic(policy_bytes)?;
    let seal_parse = add(worker_json(seal_bytes, 2)?, seal)?;
    let approval_parse = add(seal, mul(2, worker_json(approval_bytes, 1)?)?)?;
    let policy_parse = add(seal, add(worker_json(policy_bytes, 2)?, policy)?)?;
    let authorization_parse = if authorization_bytes == 0 {
        add(seal, policy)?
    } else {
        add(add(seal, policy)?, worker_json(authorization_bytes, 2)?)?
    };
    let retained_and_clone = add(
        add(add(mul(2, seal)?, policy)?, 3 * 64)?,
        mul(3, MANIFEST_BYTES)?,
    )?;
    Ok(seal_parse
        .max(approval_parse)
        .max(policy_parse)
        .max(authorization_parse)
        .max(retained_and_clone))
}

/// Repair keeps the parsed request beside one original and one Source-bound
/// seal clone. Approval bytes are hashed in place and own only the 64-byte
/// digest result.
pub(crate) fn worker_repair_documents(seal_bytes: usize, request_bytes: usize) -> Result<usize> {
    let seal = seal_dynamic()?;
    let seal_parse = add(worker_json(seal_bytes, 2)?, seal)?;
    let request = worker_json(request_bytes, 2)?;
    Ok(seal_parse.max(add(seal, request)?).max(add(
        add(mul(2, seal)?, add(request, 64)?)?,
        mul(3, MANIFEST_BYTES)?,
    )?))
}

/// Repair execution is independent of the incoming request graph. These are
/// existing 8 MiB stored-document ceilings, including invalid direct-serde
/// input before its semantic checks. The four control owners are the outer and
/// transaction-check Binding/Progress pairs. The four item owners cover old
/// Outcome + old receipt/archive + replacement Outcome + receipt verification;
/// each includes raw, typed, Content and encoder overlap rather than only the
/// eventual successful shape. The saved importer Policy and reconciliation
/// before/after cursors remain separately live during those checks.
pub(crate) fn worker_repair_execution() -> Result<usize> {
    let document = add(worker_json(RETAINED_BYTES, 2)?, mul(4, RETAINED_BYTES)?)?;
    let controls = mul(4, document)?;
    let item = mul(4, document)?;
    // Two PreparedKeywordProjection proofs can coexist with the currently
    // loading proof. Each Evidence has the established 100-record/8 MiB
    // aggregate ceiling; returned row/field clones own a second complete graph.
    let proof = add(
        mul(2, record_dynamic(RETAINED_BYTES, 100)?)?,
        tree_layout(
            100,
            std::alloc::Layout::new::<i64>(),
            crate::catalog_migration::organization::evidence_cache_entry_layout(),
        )?,
    )?;
    let proofs = mul(3, proof)?;
    let field_maps = mul(
        2,
        add(
            record_dynamic(RETAINED_BYTES, 1)?,
            retained_field_work(RETAINED_BYTES)?,
        )?,
    )?;
    // Parent archives retire one at a time, but each copied name can survive in
    // the 64-ancestor path until native comparison. Do not assume name validity
    // before that comparison. The Order query includes its 1025th refusal row.
    let hierarchy = add(mul(65, RETAINED_BYTES)?, vector::<String>(65)?)?;
    let order = add(
        tree::<i64, (Option<i64>, String)>(1025)?,
        mul(2, tree::<i64, i64>(1025)?)?,
    )?;
    // Current repair retains two compressed archives. Each Zlib output can grow
    // with its old backing present; decoding additionally owns R+1 refusal bytes.
    // flate2's pinned default 32 KiB window backends use bounded window/hash/
    // pending/Huffman workspaces, each below 32 windows including their Boxes.
    let archives = add(
        mul(2, vector::<u8>(add(RETAINED_BYTES, 65536)?)?)?,
        add(vector::<u8>(add(RETAINED_BYTES, 1)?)?, mul(32, 32768)?)?,
    )?;
    add(
        controls,
        add(
            item,
            add(
                saved_state()?,
                add(
                    proofs,
                    add(
                        selected_record_load_work()?,
                        add(field_maps, add(hierarchy, add(order, archives)?)?)?,
                    )?,
                )?,
            )?,
        )?,
    )
}

/// CatalogData is parsed before Adobe's final 8 MiB output check. Every
/// Property consumes at least one input byte, and every copied ancestor name,
/// lexical token or value originates in that same input. Include the first
/// refusal Property and 33 recursive path owners, pinned Vec growth and BTree
/// key storage. This uses actual borrowed input bytes, never a smaller cap.
pub(crate) fn repair_adobe_catalog(bytes: usize) -> Result<usize> {
    use crate::lightroom::adobe::{Key, Limits, Property};
    let limits = Limits::default();
    if bytes > limits.bytes {
        return Ok(0);
    } // existing parser returns before allocation
    let properties = add(bytes.min(limits.properties), 1)?;
    let paths = add(properties, add(limits.depth, 1)?)?;
    let path = add(vector::<Key>(add(limits.depth, 1)?)?, mul(4, bytes)?)?;
    let properties = vector::<Property>(properties)?;
    let keys = tree::<Key, ()>(bytes.min(limits.tokens))?;
    add(
        properties,
        add(
            mul(paths, path)?,
            add(
                keys,
                add(mul(4, bytes)?, vector::<u8>(limits.output_bytes)?)?,
            )?,
        )?,
    )
}

pub(crate) fn worker_supplement_documents(bytes: usize) -> Result<usize> {
    worker_json(bytes, 2)
}

/// Status queries accept at most the shared executor's existing 16 MiB stored
/// document. SQLite's raw Vec coexists with the decoded typed progress owner.
pub(crate) fn worker_status() -> Result<usize> {
    add(
        crate::catalog_migration::lightroom_executor::DOCUMENT_BYTES as usize,
        worker_json(
            crate::catalog_migration::lightroom_executor::DOCUMENT_BYTES as usize,
            2,
        )?,
    )
}

/// One supplement preparation retains the 4 MiB inspection and decoded
/// Document, both existing 64 MiB payload classes, their 1024-member outer
/// vectors/proof vectors, the 8 MiB normalized proof encoder's old/new overlap,
/// and the established evidence chunk/descriptor workspace. Prior Prepared
/// values remain live in the growing result roster.
pub(crate) fn worker_supplement_execution(requests: usize) -> Result<usize> {
    use crate::catalog_migration::{evidence, file_metadata, supplements};
    const DOCUMENT: usize = 4 * 1024 * 1024;
    const PAYLOAD_CLASS: usize = 64 * 1024 * 1024;
    const MEMBERS: usize = 1024;
    const NORMALIZED: usize = 8 * 1024 * 1024;
    const PATH: usize = 16_384;
    let document = add(DOCUMENT, worker_json(DOCUMENT, 2)?)?;
    let payloads = add(mul(2, PAYLOAD_CLASS)?, mul(2, vector::<Vec<u8>>(MEMBERS)?)?)?;
    let proof = add(
        vector::<file_metadata::SupplementPacket>(MEMBERS)?,
        vector::<file_metadata::SupplementInput>(MEMBERS)?,
    )?;
    let encoder = mul(3, NORMALIZED)?;
    let evidence = add(
        mul(5, evidence::CHUNK_BYTES)?,
        add(mul(3, DESCRIPTOR_BYTES)?, mul(4, PATH)?)?,
    )?;
    let prepared_payload = add(4096, add(mul(5, 64)?, "embedded".len())?)?;
    let prepared = add(
        vector::<supplements::Prepared>(requests)?,
        mul(requests, prepared_payload)?,
    )?;
    add(
        prepared,
        add(
            document,
            add(payloads, add(proof, add(encoder, evidence)?)?)?,
        )?,
    )
}

/// Requested backing during roxmltree 0.21.1 parsing. The pinned parser starts
/// node/attribute vectors from spelling counts, keeps its temporary vectors
/// beside the growing Document, and has DTD disabled on every admitted caller.
/// The word-count layouts are portable upper expressions for the pinned private
/// fields: NodeData <=16 words, AttributeData <=8, Namespace <=4 and
/// TempAttributeData <=10. Each vector term includes its pinned minimum capacity
/// and the old/new RawVec overlap. Owned normalized strings, Arc headers, the
/// TextBuffer/join buffer and the previous first-text owner are separate.
pub(crate) fn xml_document(text: &str) -> Result<usize> {
    let tag_spellings = text.as_bytes().iter().filter(|byte| **byte == b'<').count();
    let attribute_spellings = text.as_bytes().iter().filter(|byte| **byte == b'=').count();
    let namespace_declarations = text.match_indices("xmlns").count();
    let namespace_values = add(namespace_declarations, 1)?; // implicit xml binding
    let syntactic_nodes = add(mul(2, tag_spellings)?, 2)?;
    let words = size_of::<usize>();
    let overlapping_vector = |entries: usize, minimum: usize, item: usize| {
        mul(add(mul(3, entries.max(minimum))?, 8)?, item)
    };
    let node_backing = overlapping_vector(syntactic_nodes, tag_spellings, mul(16, words)?)?;
    let attribute_backing =
        overlapping_vector(attribute_spellings, attribute_spellings, mul(8, words)?)?;
    let namespace_backing = overlapping_vector(namespace_values, 0, mul(4, words)?)?;
    // tree_order only inherits a parent's namespace slice when the current
    // element declares a namespace (parse.rs resolve_namespaces). There are at
    // most d declaration events and d values, plus the implicit xml index.
    let tree_order = mul(namespace_values, add(namespace_values, 1)?)?;
    let namespace_indices = add(
        overlapping_vector(tree_order, 0, size_of::<u16>())?,
        overlapping_vector(namespace_values, 0, size_of::<u16>())?,
    )?;
    let temporary_attributes = overlapping_vector(attribute_spellings, 16, mul(10, words)?)?;
    // awaiting_subtree stores NodeId, parent_prefixes stores &str and after_text
    // stores Cow<str>. Their initial capacities are respectively 0, 1 and 1.
    let parser_vectors = add(
        overlapping_vector(tag_spellings, 0, words)?,
        add(
            overlapping_vector(add(tag_spellings, 1)?, 1, mul(2, words)?)?,
            overlapping_vector(syntactic_nodes, 1, mul(3, words)?)?,
        )?,
    )?;
    let normalized_owners = add(
        mul(4, text.len())?,
        mul(add(attribute_spellings, syntactic_nodes)?, mul(2, words)?)?,
    )?;
    add(
        add(node_backing, attribute_backing)?,
        add(
            add(namespace_backing, namespace_indices)?,
            add(
                temporary_attributes,
                add(parser_vectors, normalized_owners)?,
            )?,
        )?,
    )
}

/// Selected file-metadata caller owners present before Prepared starts. The
/// Evidence cache's accepted canonical bytes share R across the actual selected
/// records. Evidence retains one graph while returned/path/Historical clones
/// retain the second. The fixed extra two records are the selected file and
/// retained path; the existing call-site ceiling is 2*COUNT+4, not a new cap.
/// Reconstructed/supplemental Inspection retains the existing 64 MiB raw and
/// 64 MiB decoded payload ceilings while Prepared is built. Every vector/tree
/// uses the actual requested roster and the pinned RawVec/BTree expressions.
pub(crate) fn file_metadata_selected(packet_records: usize) -> Result<usize> {
    add(
        saved_state_retained()?,
        file_metadata_projection_work(packet_records)?,
    )
}

/// Selected projection work without the importer caller state already retained
/// by file_metadata_preprojection's outer scope.
pub(crate) fn file_metadata_projection_work(packet_records: usize) -> Result<usize> {
    use crate::lightroom::migration_source::EvidenceRecord;
    let evidence_records = add(packet_records, 2)?;
    let evidence = add(
        mul(2, record_dynamic(RETAINED_BYTES, evidence_records)?)?,
        add(
            tree_layout(
                evidence_records,
                std::alloc::Layout::new::<i64>(),
                crate::catalog_migration::organization::evidence_cache_entry_layout(),
            )?,
            vector::<(i64, EvidenceRecord)>(packet_records)?,
        )?,
    )?;
    // The serde family is the already-pinned direct R-byte historical/seal/
    // supplemental parser envelope. The returned typed graph and the raw/typed
    // overlap are named independently from the two retained packet byte sums.
    let parser_family = add(
        content_containers(RETAINED_BYTES, 1)?,
        add(mul(32, RETAINED_BYTES)?, mul(8, FRAME_BYTES)?)?,
    )?;
    let retained_inspection = add(
        mul(2, FILE_METADATA_PAYLOAD_BYTES)?,
        add(
            vector_layout(
                packet_records,
                std::alloc::Layout::new::<crate::xmp_packets::Packet>(),
            )?,
            vector_layout(
                packet_records,
                std::alloc::Layout::new::<crate::xmp_packets::ParseInput>(),
            )?,
        )?,
    )?;
    let historical = add(parser_family, add(seal_dynamic()?, retained_inspection)?)?;
    let roster = add(
        add(
            vector_layout(
                packet_records,
                crate::catalog_migration::file_metadata::packet_guard_layout(),
            )?,
            vector::<i64>(packet_records)?,
        )?,
        add(
            tree::<i64, ()>(packet_records)?,
            mul(2, tree::<usize, usize>(packet_records)?)?,
        )?,
    )?;
    add(evidence, add(historical, roster)?)
}

fn source_record_clone(
    record: &crate::catalog_migration::organization::SourceRecord,
) -> Result<usize> {
    use crate::lightroom::plan::Cell;
    let key_payload = record.source.key.iter().try_fold(0usize, |bytes, cell| {
        add(
            bytes,
            match cell {
                Cell::Text(value) | Cell::Blob(value) => value.len(),
                _ => 0,
            },
        )
    })?;
    add(
        add(
            record.source.capture_revision.len(),
            record.source.table.len(),
        )?,
        add(
            mul(record.source.key.len(), size_of::<Cell>())?,
            key_payload,
        )?,
    )
}

fn source_record_retained(
    record: &crate::catalog_migration::organization::SourceRecord,
) -> Result<usize> {
    use crate::lightroom::plan::Cell;
    let key_payload = record.source.key.iter().try_fold(0usize, |bytes, cell| {
        add(
            bytes,
            match cell {
                Cell::Text(value) | Cell::Blob(value) => value.capacity(),
                _ => 0,
            },
        )
    })?;
    add(
        add(
            record.source.capture_revision.capacity(),
            record.source.table.capacity(),
        )?,
        add(
            mul(record.source.key.capacity(), size_of::<Cell>())?,
            key_payload,
        )?,
    )
}

fn association_retained(
    association: &crate::catalog_migration::file_metadata::Association,
) -> usize {
    match association {
        crate::catalog_migration::file_metadata::Association::Unresolved => 0,
        crate::catalog_migration::file_metadata::Association::Confirmed { reason } => {
            reason.capacity()
        }
    }
}

fn projection_clone(
    file: &crate::catalog_migration::organization::SourceRecord,
    packet_records: usize,
    import_source: &str,
) -> Result<usize> {
    add(
        source_record_clone(file)?,
        add(mul(packet_records, size_of::<i64>())?, import_source.len())?,
    )
}

/// Exact heap payload allocated when the importer clones an already validated
/// projection and attaches one retained supplemental-evidence identifier.
pub(crate) fn file_metadata_supplemental_projection_clone(
    request: &crate::catalog_migration::file_metadata::Projection,
    supplement_bytes: usize,
) -> Result<usize> {
    let association = match &request.association {
        crate::catalog_migration::file_metadata::Association::Unresolved => 0,
        crate::catalog_migration::file_metadata::Association::Confirmed { reason } => reason.len(),
    };
    add(
        projection_clone(
            &request.file,
            request.packet_records.len(),
            &request.import_source,
        )?,
        add(association, supplement_bytes)?,
    )
}

fn path_lookup_page_max() -> Result<(usize, usize)> {
    use crate::catalog_migration::lookup::{LookupHit, UnavailableKey};
    // The shortest accepted classification member is
    // {"field":"","reason":"Missing"}; commas separate later members.
    const MEMBER: usize = r#"{"field":"","reason":"Missing"}"#.len();
    let unavailable = (LOOKUP_CLASSIFICATION_BYTES + 1) / (MEMBER + 1);
    let hit = add(
        vector::<UnavailableKey>(unavailable)?,
        LOOKUP_CLASSIFICATION_BYTES,
    )?;
    let returned = add(vector::<LookupHit>(2)?, add(mul(2, hit)?, 64)?)?;
    // One raw classification is parsed while earlier hits and the optional
    // cursor remain live. This is the same bounded serde family used elsewhere;
    // the two source-id query copies and four fixed 64-byte identity copies are
    // the simultaneously retained primary/availability parameter vectors.
    let parser = add(
        content_containers(LOOKUP_CLASSIFICATION_BYTES, 1)?,
        add(mul(32, LOOKUP_CLASSIFICATION_BYTES)?, mul(8, FRAME_BYTES)?)?,
    )?;
    let working = add(returned, add(parser, add(mul(2, 4096)?, mul(4, 64)?)?)?)?;
    Ok((returned, working))
}

fn source_key_identity_work(
    source: &crate::catalog_migration::originals::SourceKey,
) -> Result<usize> {
    const SOURCE: usize = r#"{"capture_revision":"","table":"","key":[]}"#.len();
    const NULL: usize = r#"{"type":"Null"}"#.len();
    const INTEGER: usize = r#"{"type":"Integer","value":-9223372036854775808}"#.len();
    const REAL: usize = r#"{"type":"RealBits","value":18446744073709551615}"#.len();
    const TEXT: usize = r#"{"type":"Text","value":""}"#.len();
    const BLOB: usize = r#"{"type":"Blob","value":""}"#.len();
    let mut encoded = add(
        SOURCE,
        mul(6, add(source.capture_revision.len(), source.table.len())?)?,
    )?;
    let mut hex = 0usize;
    for (index, cell) in source.key.iter().enumerate() {
        encoded = add(encoded, usize::from(index != 0))?;
        encoded = add(
            encoded,
            match cell {
                Cell::Null => NULL,
                Cell::Integer(_) => INTEGER,
                Cell::RealBits(_) => REAL,
                Cell::Text(value) => {
                    hex = hex.max(mul(2, value.len())?);
                    add(TEXT, mul(2, value.len())?)?
                }
                Cell::Blob(value) => {
                    hex = hex.max(mul(2, value.len())?);
                    add(BLOB, mul(2, value.len())?)?
                }
            },
        )?;
    }
    // serde_json's output Vec can grow while Cell's existing hex serializer
    // owns its one current 2-byte-per-input-byte String. The returned digest
    // String is produced only after both have dropped.
    Ok(add(vector::<u8>(encoded)?, hex)?.max(64))
}

fn selected_record_load_work() -> Result<usize> {
    let record = add(
        record_dynamic(RETAINED_BYTES, 1)?,
        vector::<Cell>(RETAINED_BYTES / 2)?,
    )?;
    // Exact selected_record opening already established for Artifact: retained
    // seal R, compressed R+32768, growing decoded raw <=2*(R+1)+8 and its old
    // <=R+1 backing. Four 64-byte SQL identities include Evidence::load's outer
    // input/digest and selected_record's inner copies.
    let opening = add(
        add(add(mul(5, RETAINED_BYTES)?, 32779)?, mul(4, 64)?)?,
        add(
            add(seal_dynamic()?, seal_validation()?)?,
            mul(3, MANIFEST_BYTES)?,
        )?,
    )?;
    let parser = add(
        content_containers(RETAINED_BYTES, 1)?,
        add(mul(32, RETAINED_BYTES)?, mul(8, FRAME_BYTES)?)?,
    )?;
    add(record, add(opening, parser)?)
}

fn retained_field_work(maximum: usize) -> Result<usize> {
    // field_bytes keeps the SQL-copied descriptor and its 64-byte evidence ID
    // through the later chunk loop. Direct ByteRef parsing additionally owns at
    // most one descriptor-sized typed string payload graph.
    let retained_descriptor = add(DESCRIPTOR_BYTES, 64)?;
    let descriptor = add(
        add(retained_descriptor, DESCRIPTOR_BYTES)?,
        add(
            content_containers(DESCRIPTOR_BYTES, 1)?,
            add(mul(32, DESCRIPTOR_BYTES)?, mul(8, FRAME_BYTES)?)?,
        )?,
    )?;
    // A Bytes field reads one existing 1 MiB evidence chunk. The stored
    // compressed member can be CHUNK+4096 before the declared short field length
    // is compared; the decoded candidate can reach maximum+1 on refusal.
    let chunk = add(
        retained_descriptor,
        add(
            add(crate::catalog_migration::evidence::CHUNK_BYTES, 4096)?,
            add(vector::<u8>(add(maximum, 1)?)?, mul(3, 64)?)?,
        )?,
    )?;
    Ok(descriptor.max(chunk))
}

fn source_id_work(file: &crate::catalog_migration::organization::SourceRecord) -> Result<usize> {
    const TABLE: usize = 1024;
    const KEY: usize = 64 * 1024;
    const SOURCE_ID: usize = 4096;
    let identity = source_key_identity_work(&file.source)?;
    let load = selected_record_load_work()?;
    let record = record_dynamic(RETAINED_BYTES, 1)?;
    let cache = add(
        record,
        add(
            tree_layout(
                1,
                std::alloc::Layout::new::<i64>(),
                crate::catalog_migration::organization::evidence_cache_entry_layout(),
            )?,
            mul(2, 64)?,
        )?,
    )?;
    let table = vector::<u8>(TABLE)?;
    let key = vector::<u8>(KEY)?;
    let source_id = vector::<u8>(SOURCE_ID)?;
    // The shortest Cell is {"type":"Null"}; a nonempty byte Cell needs at
    // least {"type":"Text","value":"00"}. Separators cost one byte except
    // after the last member. Decode owns the root Vec, every completed nested
    // byte Vec, and one current hex String. Sum the nested RawVec growth from
    // the KEY/2 decoded-byte ceiling and give every possible nonempty vector its
    // pinned per-vector growth term instead of relying on unused Cell roots.
    const CELL_MEMBER: usize = r#"{"type":"Null"}"#.len();
    const BYTE_MEMBER: usize = r#"{"type":"Text","value":"00"}"#.len();
    let cells = (KEY + 1) / (CELL_MEMBER + 1);
    let byte_vectors = (KEY + 1) / (BYTE_MEMBER + 1);
    let decoded_payload = add(KEY, mul(8, byte_vectors)?)?;
    let hex_scratch = vector::<u8>(KEY)?;
    let key_parser = add(
        add(vector::<Cell>(cells)?, add(decoded_payload, hex_scratch)?)?,
        add(
            content_containers(KEY, 1)?,
            add(mul(32, KEY)?, mul(8, FRAME_BYTES)?)?,
        )?,
    )?;
    // Evidence::source keeps its cache and cloned record through each field.
    // Table stays live while key loads/parses, then both stay live while the
    // returned source ID is assembled. Only one field's descriptor/chunk work
    // exists at a time, so take the phase maximum rather than summing it.
    let fields = add(table, retained_field_work(TABLE)?)?
        .max(add(add(table, key)?, retained_field_work(KEY)?)?)
        .max(add(add(table, key)?, key_parser)?)
        .max(add(
            add(add(table, key)?, source_id)?,
            retained_field_work(SOURCE_ID)?,
        )?);
    Ok(identity.max(load).max(add(cache, add(record, fields)?)?))
}

/// Importer construction high water admitted before Walk::new. It includes the
/// existing source roster/lookup bounds and construction of the first Projection.
/// The scope is then shrunk to the capacities of the returned owner graph. Later
/// content-dependent projection work shares its Requested ledger and is admitted
/// before allocation while these owners remain live.
pub(crate) fn file_metadata_preprojection(
    file: &crate::catalog_migration::organization::SourceRecord,
    import_source: &str,
    association_reason: Option<&str>,
) -> Result<usize> {
    let (page, page_work) = path_lookup_page_max()?;
    let association = association_reason.map_or(0, str::len);
    let persistent = add(association, add(4096, page)?)?;
    let roster = vector::<i64>(FILE_METADATA_PACKET_RECORDS)?;
    let packet_records = mul(FILE_METADATA_PACKET_RECORDS, size_of::<i64>())?;
    let roster_phase = add(persistent, add(roster, packet_records)?)?;
    let request_phase = add(
        persistent,
        projection_clone(file, FILE_METADATA_PACKET_RECORDS, import_source)?,
    )?;
    let retained = add(
        add(4096, page)?,
        add(
            association,
            projection_clone(file, FILE_METADATA_PACKET_RECORDS, import_source)?,
        )?,
    )?;
    let local = add(association, source_id_work(file)?)?
        .max(add(association, add(4096, page_work)?)?)
        .max(roster_phase)
        .max(request_phase)
        .max(retained);
    add(saved_state_retained()?, local)
}

/// Requested heap storage that remains after importer construction. Capacities
/// are read only after the preprojection maximum has already been admitted.
pub(crate) fn file_metadata_preprojection_retained(
    source_id: &String,
    page: &crate::catalog_migration::lookup::LookupPage,
    request: &crate::catalog_migration::file_metadata::Projection,
) -> Result<usize> {
    use crate::catalog_migration::lookup::{LookupHit, UnavailableKey};
    let records = mul(page.records.capacity(), size_of::<LookupHit>())?;
    let hits = page.records.iter().try_fold(0usize, |bytes, hit| {
        let fields = hit
            .unavailable
            .iter()
            .try_fold(0usize, |sum, value| add(sum, value.field.capacity()))?;
        add(
            bytes,
            add(
                mul(hit.unavailable.capacity(), size_of::<UnavailableKey>())?,
                fields,
            )?,
        )
    })?;
    let cursor = page
        .next
        .as_ref()
        .map_or(0, |value| value.query_blake3.capacity());
    let page = add(records, add(hits, cursor)?)?;
    let projection = add(
        source_record_retained(&request.file)?,
        add(
            mul(request.packet_records.capacity(), size_of::<i64>())?,
            add(
                request.import_source.capacity(),
                add(
                    association_retained(&request.association),
                    request.supplement.as_ref().map_or(0, String::capacity),
                )?,
            )?,
        )?,
    )?;
    add(
        saved_state_retained()?,
        add(source_id.capacity(), add(page, projection)?)?,
    )
}

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
    fn file_metadata_preprojection_covers_construction_and_exact_supplement_clone() -> Result<()> {
        use crate::{
            catalog_migration::{
                file_metadata::{Association, Origin, Projection},
                lookup::{LookupHit, LookupPage, UnavailableKey, UnavailableReason},
                organization::SourceRecord,
                originals::SourceKey,
            },
            lightroom::plan::Cell,
        };
        let file = SourceRecord {
            retained_record: 1,
            source: SourceKey {
                capture_revision: "a".repeat(64),
                table: "AgLibraryFile".into(),
                key: vec![Cell::Text("é".repeat(40).into_bytes())],
            },
        };
        let reason = "Retained embedded packet belongs to its exact selected file source";
        let admitted = file_metadata_preprojection(&file, "lightroom", Some(reason))?;
        let page = LookupPage {
            records: vec![LookupHit {
                sequence: 2,
                unavailable: vec![UnavailableKey {
                    field: "source_id".into(),
                    reason: UnavailableReason::Missing,
                }],
            }],
            next: None,
            coverage_complete: true,
            keys_complete: true,
        };
        let request = Projection {
            file,
            retained_path: 2,
            origin: Origin::Embedded,
            packet_records: vec![3, 4],
            import_source: "lightroom".into(),
            association: Association::Confirmed {
                reason: reason.into(),
            },
            supplement: None,
        };
        let source_id = String::from("selected:file");
        let retained = file_metadata_preprojection_retained(&source_id, &page, &request)?;
        assert!(retained <= admitted);
        let selected = file_metadata_projection_work(request.packet_records.len())?;
        let supplemental_clone = file_metadata_supplemental_projection_clone(&request, 64)?;
        let overlap = add(retained, add(selected, add(supplemental_clone, selected)?)?)?;
        assert!(overlap > add(retained, selected)?);
        let source_id = source_id_work(&request.file)?;
        assert!(source_id >= selected_record_load_work()?);
        assert!(source_id >= source_key_identity_work(&request.file.source)?);
        println!(
            "CORE_FILE_METADATA_PREPROJECTION initial={admitted} source_id={source_id} retained={retained} selected={selected} supplemental_clone={supplemental_clone} shared_ledger_overlap={overlap} construction_before_walk=true roster_limit={FILE_METADATA_PACKET_RECORDS}"
        );
        Ok(())
    }

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

    #[test]
    fn lm_executor_batch3_raw_typed_source_and_supplement_phases_share_one_pool() -> Result<()> {
        use crate::lightroom_migration_worker::memory::{MemoryBudget, ResourceLimit};
        let seal = 31_337;
        let approval = 17_003;
        let policy = 11_111;
        let raw = add(seal, add(approval, policy)?)?;
        let typed = worker_run_documents(seal, approval, policy, 0)?;
        let budget = MemoryBudget::new(add(raw, typed)?)?;
        let mut documents = budget.reservation();
        documents.grow(raw)?;
        let mut competing = budget.reservation();
        competing.grow(1)?;
        let mut operation = budget.reservation();
        let error = operation.grow(typed).unwrap_err();
        let limit = error.downcast_ref::<ResourceLimit>().unwrap();
        assert_eq!((limit.required, limit.available), (typed, typed - 1));
        drop(competing);
        operation.grow(typed)?;
        assert_eq!(budget.used(), add(raw, typed)?);
        assert!(worker_supplement_execution(1024)? > 2 * 64 * 1024 * 1024);
        assert!(worker_status()? > 16 * 1024 * 1024);
        drop(operation);
        assert_eq!(budget.used(), raw);
        drop(documents);
        assert_eq!(budget.used(), 0);
        Ok(())
    }
}

//! Source-bound catalog XMP and current Adobe recipe projection.
//! Complete raw rows remain authoritative even when interpretation is unavailable.
use super::{
    images,
    organization::{Evidence, Link, SourceRecord, verify_unique_link},
    retention,
};
use crate::{
    Catalog,
    catalog_images::{self, TranslationState},
    catalog_metadata::{self, Prepared, Source},
    catalog_writer::Priority,
    lightroom::{
        adobe,
        migration_source::{Collection, MigrationSource},
        plan::Cell,
    },
    xmp_packets::{self, Inspection, Packet, ParseInput, SourceRevision, Status},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const LIMIT: usize = 8 * 1024 * 1024;
const ADAPTER: &str = "lightroom-native-metadata-v1";
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogXmp {
    pub origin: SourceRecord,
    pub retained_table: i64,
    pub packet_record: i64,
    pub image: Link,
    pub import_source: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CurrentDevelop {
    pub image: SourceRecord,
    pub image_table: i64,
    pub settings: Link,
    pub settings_table: i64,
    pub settings_path: Vec<adobe::Key>,
    pub import_source: String,
    /// Initial import normally uses zero. An existing user edit requires an
    /// explicit revision decision instead of being silently overwritten.
    pub expected_edit_revision: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultRecord {
    pub input_digest: String,
    pub image: crate::catalog_edits::VariantKey,
    pub state: String,
    pub observation: Option<i64>,
    pub edit_revision: Option<i64>,
    pub reason: Option<String>,
    pub extraction: Option<adobe::Extraction>,
}
pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS migration_metadata(
        image_source TEXT NOT NULL, payload_source TEXT NOT NULL, slot TEXT NOT NULL,
        owner TEXT NOT NULL, input_digest TEXT NOT NULL,
        retained_record INTEGER NOT NULL REFERENCES migration_retained_records(sequence),
        result BLOB NOT NULL, PRIMARY KEY(image_source,payload_source,slot));",
    )?;
    Ok(())
}
fn owner(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 4096 && !value.contains('\0'),
        "metadata import owner bounds"
    );
    Ok(())
}
fn encoded<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    crate::lightroom::bounded_json(value, LIMIT)
}
fn digest(value: &serde_json::Value) -> Result<String> {
    Ok(blake3::hash(&encoded(value)?).to_hex().to_string())
}
fn previous(
    db: &Connection,
    image: &str,
    payload: &str,
    slot: &str,
    owner: &str,
    digest: &str,
) -> Result<Option<ResultRecord>> {
    let old:Option<(String,String,Vec<u8>)>=db.query_row("SELECT owner,input_digest,result FROM migration_metadata WHERE image_source=?1 AND payload_source=?2 AND slot=?3",params![image,payload,slot],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    old.map(|(old_owner, old_digest, bytes)| {
        ensure!(
            old_owner == owner && old_digest == digest,
            "metadata projection decision changed; explicit reconciliation required"
        );
        ensure!(bytes.len() <= LIMIT, "metadata projection result bounds");
        Ok(serde_json::from_slice(&bytes)?)
    })
    .transpose()
}
fn save(
    db: &Connection,
    image: &str,
    payload: &str,
    slot: &str,
    owner: &str,
    record: i64,
    result: &ResultRecord,
) -> Result<()> {
    db.execute(
        "INSERT INTO migration_metadata VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![
            image,
            payload,
            slot,
            owner,
            result.input_digest,
            record,
            encoded(result)?
        ],
    )?;
    Ok(())
}
fn kind(fields: &BTreeMap<String, Cell>) -> adobe::SourceKind {
    let Some(Cell::Text(format)) = fields.get("fileFormat") else {
        return adobe::SourceKind::Unknown;
    };
    match format.as_slice() {
        b"RAW" | b"DNG" | b"CR2" | b"CR3" | b"NEF" | b"ARW" | b"RAF" => adobe::SourceKind::Raw,
        b"JPEG" | b"JPG" | b"PNG" | b"TIFF" | b"PSD" | b"AVIF" | b"WEBP" | b"BMP" => {
            adobe::SourceKind::Raster
        }
        _ => adobe::SourceKind::Unknown,
    }
}
fn fields_fit(record: &crate::lightroom::migration_source::EvidenceRecord, names: &[&str]) -> bool {
    names.iter().all(|name| match record.fields.get(*name) {
        Some(crate::lightroom::migration_source::Field::Bytes(r)) => r.bytes <= LIMIT as u64,
        Some(crate::lightroom::migration_source::Field::Inline(Cell::Text(v) | Cell::Blob(v))) => {
            v.len() <= LIMIT
        }
        _ => true,
    })
}
fn row_fits(
    db: &Connection,
    proof: &mut Evidence,
    origin: &SourceRecord,
    table: i64,
) -> Result<bool> {
    images::table_proof(db, proof, origin, table)?;
    Ok(
        fields_fit(&proof.record(db, origin.retained_record)?, &["cells_json"])
            && fields_fit(&proof.record(db, table)?, &["columns_json"]),
    )
}
struct RetainedDecision<'a> {
    image: &'a SourceRecord,
    payload: &'a SourceRecord,
    owner: &'a str,
    slot: &'a str,
    digest: String,
    reason: &'a str,
}
fn retain_only(
    catalog: &mut Catalog,
    proof: &Evidence,
    request: RetainedDecision<'_>,
) -> Result<ResultRecord> {
    let image = request.image.source.identity()?;
    let payload = request.payload.source.identity()?;
    let key = images::mapped_image(&catalog.db, request.owner, &request.image.source)?;
    let result = ResultRecord {
        input_digest: request.digest,
        image: key.clone(),
        state: "retained_only".into(),
        observation: None,
        edit_revision: None,
        reason: Some(request.reason.into()),
        extraction: None,
    };
    let _permit = catalog.writers.enter(Priority::Background)?;
    let tx = catalog
        .db
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    proof.recheck(&tx)?;
    ensure!(
        images::mapped_image(&tx, request.owner, &request.image.source)? == key,
        "retained metadata image mapping changed"
    );
    if let Some(previous) = previous(
        &tx,
        &image,
        &payload,
        request.slot,
        request.owner,
        &result.input_digest,
    )? {
        tx.commit()?;
        return Ok(previous);
    }
    save(
        &tx,
        &image,
        &payload,
        request.slot,
        request.owner,
        request.payload.retained_record,
        &result,
    )?;
    tx.commit()?;
    Ok(result)
}

impl Catalog {
    /// Retain the exact catalog XMP carrier plus its independently checked parse
    /// input. A zlib wrapper never replaces the original typed payload bytes.
    pub fn project_migration_catalog_xmp(
        &mut self,
        source: Option<&MigrationSource>,
        request: &CatalogXmp,
    ) -> Result<ResultRecord> {
        owner(&request.import_source)?;
        ensure!(
            request.origin.source.table == "Adobe_AdditionalMetadata"
                && request.image.field == "image"
                && request.image.target.source.table == "Adobe_images",
            "catalog XMP source/link differs"
        );
        let payload = request.origin.source.identity()?;
        let image = request.image.target.source.identity()?;
        let input_digest = digest(
            &serde_json::json!({"adapter":ADAPTER,"owner":request.import_source,"image":request.image.target.source,"payload":request.origin.source,"slot":"catalog_xmp"}),
        )?;
        let mut proof = Evidence::default();
        let source_id = proof.source(&self.db, &request.origin)?;
        proof.source(&self.db, &request.image.target)?;
        let key = images::mapped_image(
            &self.db,
            &request.import_source,
            &request.image.target.source,
        )?;
        if let Some(result) = previous(
            &self.db,
            &image,
            &payload,
            "catalog_xmp",
            &request.import_source,
            &input_digest,
        )? {
            return Ok(result);
        }
        verify_unique_link(
            &self.db,
            &mut proof,
            &request.origin,
            &request.image,
            source.context("first XMP projection needs sealed source")?,
        )?;
        let packet = proof.record(&self.db, request.packet_record)?;
        proof.same_input(request.origin.retained_record, request.packet_record)?;
        ensure!(
            packet.collection == Collection::Packets
                && packet.revision == request.origin.source.capture_revision,
            "catalog XMP packet capture differs"
        );
        ensure!(
            retention::field_bytes(&self.db, request.packet_record, &packet, "source_id", 4096)?
                == source_id.as_bytes()
                && retention::field_bytes(&self.db, request.packet_record, &packet, "origin", 64)?
                    == b"catalog",
            "catalog XMP packet association differs"
        );
        let row_fits = row_fits(
            &self.db,
            &mut proof,
            &request.origin,
            request.retained_table,
        )?;
        if !row_fits || !fields_fit(&packet, &["raw", "decoded"]) {
            return retain_only(
                self,
                &proof,
                RetainedDecision {
                    image: &request.image.target,
                    payload: &request.origin,
                    owner: &request.import_source,
                    slot: "catalog_xmp",
                    digest: input_digest,
                    reason: "Catalog XMP interpretation exceeds the bounded row/packet limit; complete original bytes remain in custody",
                },
            );
        }
        let fields = images::columns(
            &self.db,
            &mut proof,
            &request.origin,
            request.retained_table,
        )?;
        let raw = retention::field_bytes(&self.db, request.packet_record, &packet, "raw", LIMIT)?;
        let hash = blake3::hash(&raw).to_hex().to_string();
        ensure!(
            retention::field_bytes(&self.db, request.packet_record, &packet, "raw_digest", 64)?
                == hash.as_bytes(),
            "catalog XMP raw digest differs"
        );
        let decoded = match fields.get("xmp").context("catalog XMP column absent")? {
            Cell::Text(bytes) => {
                ensure!(bytes == &raw, "catalog XMP typed text differs");
                Ok((bytes.clone(), xmp_packets::Transformation::Identity))
            }
            Cell::Blob(bytes) => {
                ensure!(bytes == &raw, "catalog XMP typed blob differs");
                crate::lightroom::plan::decode_catalog_xmp(bytes, LIMIT)
                    .map(|v| (v, xmp_packets::Transformation::CatalogLengthPrefixedZlib))
            }
            _ => anyhow::bail!("catalog XMP cell is not bytes"),
        };
        let mut result = ResultRecord {
            input_digest,
            image: key.clone(),
            state: "retained_only".into(),
            observation: None,
            edit_revision: None,
            reason: None,
            extraction: None,
        };
        let prepared = match decoded {
            Ok((decoded, transformation)) => {
                ensure!(
                    retention::field_bytes(
                        &self.db,
                        request.packet_record,
                        &packet,
                        "decoded",
                        LIMIT
                    )? == decoded,
                    "catalog XMP retained parse input differs"
                );
                let metadata_source = Source {
                    kind: "catalog".into(),
                    locator: format!("lightroom:{payload}:xmp").into_bytes(),
                    display: "Lightroom catalog XMP".into(),
                    ambiguous: false,
                    provenance: serde_json::json!({"adapter":ADAPTER,"source":request.origin.source,"packet_record":request.packet_record,"raw_blake3":hash}),
                };
                let inspection = Inspection {
                    revision: SourceRevision {
                        length: raw.len() as u64,
                        blake3: hash.clone(),
                        modified_unix_ns: None,
                    },
                    status: Status::Complete,
                    issues: vec![],
                    packets: vec![Packet {
                        container: xmp_packets::Container::CatalogXmp,
                        bytes: raw,
                        blake3: hash,
                        ranges: vec![],
                        group: payload.clone(),
                        attributes: BTreeMap::new(),
                    }],
                    parse_inputs: vec![ParseInput {
                        blake3: blake3::hash(&decoded).to_hex().to_string(),
                        bytes: decoded,
                        packet_indices: vec![0],
                        transformation,
                        group: payload.clone(),
                    }],
                };
                Some((
                    Prepared::new(&inspection, &metadata_source)?,
                    metadata_source,
                ))
            }
            Err(error) => {
                result.reason = Some(format!("catalog XMP decode retained only: {error:#}"));
                None
            }
        };
        let expected = self.image_metadata_identity(&key)?;
        let _permit = self.writers.enter(Priority::Background)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        proof.recheck(&tx)?;
        if let Some(result) = previous(
            &tx,
            &image,
            &payload,
            "catalog_xmp",
            &request.import_source,
            &result.input_digest,
        )? {
            tx.commit()?;
            return Ok(result);
        }
        ensure!(
            images::mapped_image(&tx, &request.import_source, &request.image.target.source)? == key,
            "XMP native mapping changed"
        );
        catalog_images::require_image_metadata_identity(&tx, &expected)?;
        if let Some((prepared, metadata_source)) = prepared {
            let change = catalog_metadata::retain_prepared(
                &tx,
                &expected.image_id,
                &metadata_source,
                &prepared,
                false,
            )?;
            result.observation = Some(change.observation_id);
            result.state = "metadata_retained".into();
        }
        save(
            &tx,
            &image,
            &payload,
            "catalog_xmp",
            &request.import_source,
            request.origin.retained_record,
            &result,
        )?;
        tx.commit()?;
        Ok(result)
    }

    /// Coordinator/repair selection only. Explicit nonempty caller paths remain
    /// authoritative. Parse/limit/unknown wrappers keep the old retained path;
    /// the ordinary extractor still records its failure or missing properties.
    pub(crate) fn prepare_migration_current_develop(
        &self,
        mut request: CurrentDevelop,
    ) -> Result<CurrentDevelop> {
        if !request.settings_path.is_empty() {
            return Ok(request);
        }
        let mut proof = Evidence::default();
        proof.source(&self.db, &request.settings.target)?;
        if !row_fits(
            &self.db,
            &mut proof,
            &request.settings.target,
            request.settings_table,
        )? {
            return Ok(request);
        }
        let fields = images::columns(
            &self.db,
            &mut proof,
            &request.settings.target,
            request.settings_table,
        )?;
        if let Some(Cell::Text(bytes) | Cell::Blob(bytes)) = fields.get("text")
            && let Ok(path) = adobe::catalog_settings_path(bytes, adobe::Limits::default())?
        {
            request.settings_path = path;
        }
        Ok(request)
    }
    /// The current image->develop pointer is mandatory. Historical settings are
    /// retained separately and never silently replace the current native recipe.
    pub fn project_migration_current_develop(
        &mut self,
        source: Option<&MigrationSource>,
        request: &CurrentDevelop,
    ) -> Result<ResultRecord> {
        validate_current_develop(request)?;
        let image = request.image.source.identity()?;
        let payload = request.settings.target.source.identity()?;
        let input_digest = current_develop_input_digest(request)?;
        let mut proof = Evidence::default();
        proof.source(&self.db, &request.image)?;
        proof.source(&self.db, &request.settings.target)?;
        let _key = images::mapped_image(&self.db, &request.import_source, &request.image.source)?;
        if let Some(result) = previous(
            &self.db,
            &image,
            &payload,
            "current_develop",
            &request.import_source,
            &input_digest,
        )? {
            return Ok(result);
        }
        drop(proof);
        let prepared = self.prepare_current_develop_projection(
            source.context("first current settings projection needs sealed source")?,
            request,
        )?;
        let _permit = self.writers.enter(Priority::Background)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        prepared.proof.recheck(&tx)?;
        if let Some(result) = previous(
            &tx,
            prepared.image_source(),
            prepared.payload_source(),
            "current_develop",
            &request.import_source,
            prepared.input_digest(),
        )? {
            tx.commit()?;
            return Ok(result);
        }
        let result = commit_current_develop_projection(&tx, &prepared)?;
        tx.commit()?;
        Ok(result)
    }

    /// Bounded read preparation without receipt replay/adoption. The caller must
    /// preserve the exact request and commit through the guarded helper below.
    pub(crate) fn prepare_current_develop_projection(
        &self,
        source: &MigrationSource,
        request: &CurrentDevelop,
    ) -> Result<PreparedCurrentDevelop> {
        validate_current_develop(request)?;
        let image = request.image.source.identity()?;
        let payload = request.settings.target.source.identity()?;
        let input_digest = current_develop_input_digest(request)?;
        let mut proof = Evidence::default();
        proof.source(&self.db, &request.image)?;
        proof.source(&self.db, &request.settings.target)?;
        let key = images::mapped_image(&self.db, &request.import_source, &request.image.source)?;
        verify_unique_link(
            &self.db,
            &mut proof,
            &request.image,
            &request.settings,
            source,
        )?;
        let image_fits = row_fits(&self.db, &mut proof, &request.image, request.image_table)?;
        let settings_fit = row_fits(
            &self.db,
            &mut proof,
            &request.settings.target,
            request.settings_table,
        )?;
        if !image_fits || !settings_fit {
            return Ok(PreparedCurrentDevelop {
                request: request.clone(), image, payload, proof, application: None,
                result: ResultRecord {
                    input_digest, image: key, state: "retained_only".into(), observation: None,
                    edit_revision: None, extraction: None,
                    reason: Some("Current settings interpretation exceeds the bounded row limit; complete original bytes remain in custody".into()),
                },
            });
        }
        let image_fields =
            images::columns(&self.db, &mut proof, &request.image, request.image_table)?;
        let settings_fields = images::columns(
            &self.db,
            &mut proof,
            &request.settings.target,
            request.settings_table,
        )?;
        let body = match settings_fields.get("text") {
            Some(Cell::Text(bytes) | Cell::Blob(bytes)) => Some(bytes),
            _ => None,
        };
        let extraction = body
            .map(|bytes| {
                adobe::extract(
                    bytes,
                    adobe::Input {
                        source_id: payload.clone(),
                        revision: request.settings.target.source.capture_revision.clone(),
                        locator: format!("{payload}/text"),
                        payload_blake3: blake3::hash(bytes).to_hex().to_string(),
                        payload_bytes: bytes.len() as u64,
                        format: adobe::Format::CatalogData,
                        source_kind: kind(&image_fields),
                        association: adobe::Association::Current,
                        settings_path: request.settings_path.clone(),
                        as_shot_available: false,
                    },
                    adobe::Limits::default(),
                )
            })
            .transpose()?;
        let contribution = extraction
            .as_ref()
            .map(|e| e.contribution.clone())
            .unwrap_or_default();
        let translated = contribution.exposure_ev.is_some() || contribution.white_balance.is_some();
        let current = self.edit_variant(&key)?;
        ensure!(
            current.revision == request.expected_edit_revision,
            "image was edited before settings projection; explicit reconciliation required"
        );
        let recipe = contribution.apply_to(&current.recipe)?;
        let expected = self.image_metadata_identity(&key)?;
        let result = ResultRecord {
            input_digest,
            image: key.clone(),
            state: if translated {
                "translated_with_appearance_gaps"
            } else {
                "retained_only"
            }
            .into(),
            observation: None,
            edit_revision: None,
            reason: if body.is_none() {
                Some("No interpretable current settings text column; original row retained".into())
            } else {
                Some("Adobe original payload and unsupported settings retained; native rendering is not Adobe rendering equivalence".into())
            },
            extraction,
        };
        encoded(&result)?;
        Ok(PreparedCurrentDevelop {
            request: request.clone(),
            image,
            payload,
            proof,
            result,
            application: Some((recipe, expected)),
        })
    }
}

fn validate_current_develop(request: &CurrentDevelop) -> Result<()> {
    owner(&request.import_source)?;
    ensure!(
        request.image.source.table == "Adobe_images"
            && request.settings.field == "developSettingsIDCache"
            && request.settings.target.source.table == "Adobe_imageDevelopSettings",
        "current settings source/link differs"
    );
    ensure!(
        request.expected_edit_revision >= 0 && request.settings_path.len() <= 64,
        "current settings decision bounds"
    );
    for key in &request.settings_path {
        match key {
            adobe::Key::Name(v) => ensure!(v.len() <= 4096, "settings path name bounds"),
            adobe::Key::Xml {
                namespace, name, ..
            } => ensure!(
                namespace.len() <= 4096 && name.len() <= 4096,
                "settings XML path bounds"
            ),
            adobe::Key::Index(_) => (),
        }
    }
    Ok(())
}
pub(crate) fn current_develop_input_digest(request: &CurrentDevelop) -> Result<String> {
    validate_current_develop(request)?;
    digest(
        &serde_json::json!({"adapter":ADAPTER,"owner":request.import_source,"image":request.image.source,"payload":request.settings.target.source,"slot":"current_develop","settings_path":request.settings_path,"expected_edit_revision":request.expected_edit_revision}),
    )
}
/// Owns the exact selected-custody proof and validated recipe; no public fields
/// permit replacing its request, recipe, or expected identity after preparation.
pub(crate) struct PreparedCurrentDevelop {
    request: CurrentDevelop,
    image: String,
    payload: String,
    proof: Evidence,
    result: ResultRecord,
    application: Option<(
        crate::edit::ValidatedRecipe,
        catalog_images::ImageMetadataIdentity,
    )>,
}
impl PreparedCurrentDevelop {
    pub(crate) fn result(&self) -> &ResultRecord {
        &self.result
    }
    pub(crate) fn image_source(&self) -> &str {
        &self.image
    }
    pub(crate) fn payload_source(&self) -> &str {
        &self.payload
    }
    pub(crate) fn key(&self) -> &crate::catalog_edits::VariantKey {
        &self.result.image
    }
    pub(crate) fn input_digest(&self) -> &str {
        &self.result.input_digest
    }
}
/// Caller supplies one writer transaction encompassing receipt INSERT and any
/// adoption archive/ledger/cursor CAS. Never overwrites an existing receipt.
pub(crate) fn commit_current_develop_projection(
    tx: &Connection,
    prepared: &PreparedCurrentDevelop,
) -> Result<ResultRecord> {
    ensure!(
        !tx.is_autocommit(),
        "current projection requires a caller transaction"
    );
    prepared.proof.recheck(tx)?;
    let request = &prepared.request;
    ensure!(
        images::mapped_image(tx, &request.import_source, &request.image.source)? == *prepared.key(),
        "develop native mapping changed"
    );
    let mut result = prepared.result().clone();
    if let Some((recipe, expected)) = &prepared.application {
        catalog_images::require_image_metadata_identity(tx, expected)?;
        let applied = crate::catalog_edits::install_import_recipe(
            tx,
            prepared.key(),
            request.expected_edit_revision,
            recipe,
            &serde_json::json!({"adapter":ADAPTER,"source":request.settings.target.source,"retained_record":request.settings.target.retained_record,"input_digest":result.input_digest}),
            if result.state == "translated_with_appearance_gaps" {
                TranslationState::Translated
            } else {
                TranslationState::RetainedOnly
            },
        )?;
        result.edit_revision = Some(applied.revision);
    }
    save(
        tx,
        prepared.image_source(),
        prepared.payload_source(),
        "current_develop",
        &request.import_source,
        request.settings.target.retained_record,
        &result,
    )?;
    Ok(result)
}

//! Persistent, bounded relink planning. Only catalog rows are changed; originals are read-only.
use crate::{
    Catalog, location_bytes,
    storage_volume::{
        self, LocationState, LogicalVolume, MountMatch, MountSnapshot, NativePath,
        PersistentVolumeId, VolumeLocation,
    },
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    path::Path,
};

pub(crate) const SCHEMA: &str = "
CREATE TABLE storage_epoch(id INTEGER PRIMARY KEY CHECK(id=1), revision INTEGER NOT NULL);
INSERT INTO storage_epoch VALUES(1,0);
CREATE TABLE storage_volumes(id TEXT PRIMARY KEY, identity TEXT UNIQUE);
CREATE TABLE storage_bindings(asset_id TEXT PRIMARY KEY REFERENCES assets(id), reference TEXT NOT NULL, native_path TEXT NOT NULL, volume_id TEXT REFERENCES storage_volumes(id), relative TEXT, file_key TEXT);
CREATE INDEX storage_binding_relative ON storage_bindings(volume_id,relative);
CREATE INDEX storage_binding_object ON storage_bindings(file_key);
CREATE TABLE storage_plans(id TEXT PRIMARY KEY, request TEXT NOT NULL, epoch INTEGER NOT NULL, cursor INTEGER NOT NULL DEFAULT 0, high_water INTEGER NOT NULL, state TEXT NOT NULL, applied_epoch INTEGER);
CREATE TABLE storage_heads(asset_id TEXT PRIMARY KEY REFERENCES assets(id), plan TEXT NOT NULL REFERENCES storage_plans(id));
CREATE TABLE storage_applied_items(plan TEXT NOT NULL REFERENCES storage_plans(id), asset_id TEXT NOT NULL REFERENCES assets(id), previous TEXT REFERENCES storage_plans(id), predecessor_valid INTEGER NOT NULL, after_state TEXT NOT NULL, PRIMARY KEY(plan,asset_id));
CREATE TABLE storage_exceptions(plan TEXT NOT NULL REFERENCES storage_plans(id), kind TEXT NOT NULL, entity TEXT NOT NULL, candidates TEXT NOT NULL, PRIMARY KEY(plan,kind,entity));
CREATE TABLE storage_items(plan TEXT NOT NULL REFERENCES storage_plans(id), sequence INTEGER NOT NULL, asset_id TEXT NOT NULL REFERENCES assets(id), status TEXT NOT NULL, detail TEXT NOT NULL, destination BLOB, file_key TEXT, data TEXT NOT NULL, PRIMARY KEY(plan,sequence), UNIQUE(plan,asset_id));
CREATE INDEX storage_item_destination ON storage_items(plan,destination);
CREATE INDEX storage_item_object ON storage_items(plan,file_key);
CREATE TABLE storage_source_locators(source_id INTEGER PRIMARY KEY REFERENCES metadata_sources(id),tag TEXT NOT NULL);
CREATE TABLE storage_source_items(plan TEXT NOT NULL, sequence INTEGER NOT NULL, source_id INTEGER NOT NULL REFERENCES metadata_sources(id), status TEXT NOT NULL, detail TEXT NOT NULL, destination BLOB, data TEXT NOT NULL, PRIMARY KEY(plan,source_id), FOREIGN KEY(plan,sequence) REFERENCES storage_items(plan,sequence));
CREATE INDEX storage_source_parent ON storage_source_items(plan,sequence,source_id);
CREATE INDEX storage_source_destination ON storage_source_items(plan,sequence,destination);
CREATE TRIGGER storage_asset_insert AFTER INSERT ON assets BEGIN UPDATE storage_epoch SET revision=revision+1; END;
CREATE TRIGGER storage_asset_delete AFTER DELETE ON assets BEGIN UPDATE storage_epoch SET revision=revision+1; END;
CREATE TRIGGER storage_asset_change AFTER UPDATE OF location,fingerprint ON assets WHEN OLD.location IS NOT NEW.location OR OLD.fingerprint IS NOT NEW.fingerprint BEGIN UPDATE storage_epoch SET revision=revision+1; END;
CREATE TRIGGER storage_binding_insert AFTER INSERT ON storage_bindings BEGIN UPDATE storage_epoch SET revision=revision+1; END;
CREATE TRIGGER storage_binding_update AFTER UPDATE ON storage_bindings BEGIN UPDATE storage_epoch SET revision=revision+1; END;
CREATE TRIGGER storage_binding_delete AFTER DELETE ON storage_bindings BEGIN UPDATE storage_epoch SET revision=revision+1; END;
CREATE TRIGGER storage_source_locator_insert AFTER INSERT ON storage_source_locators BEGIN UPDATE storage_epoch SET revision=revision+1; END;
CREATE TRIGGER storage_source_locator_update AFTER UPDATE ON storage_source_locators BEGIN UPDATE storage_epoch SET revision=revision+1; END;
CREATE TRIGGER storage_source_locator_delete AFTER DELETE ON storage_source_locators BEGIN UPDATE storage_epoch SET revision=revision+1; END;
CREATE TRIGGER storage_source_insert AFTER INSERT ON metadata_sources BEGIN UPDATE storage_epoch SET revision=revision+1; END;
CREATE TRIGGER storage_source_update AFTER UPDATE ON metadata_sources WHEN OLD.asset_id IS NOT NEW.asset_id OR OLD.kind IS NOT NEW.kind OR OLD.locator IS NOT NEW.locator OR OLD.display IS NOT NEW.display OR OLD.association IS NOT NEW.association OR OLD.availability IS NOT NEW.availability OR OLD.current_observation IS NOT NEW.current_observation BEGIN UPDATE storage_epoch SET revision=revision+1; END;
CREATE TRIGGER storage_source_delete AFTER DELETE ON metadata_sources BEGIN UPDATE storage_epoch SET revision=revision+1; END;
";

/// Foreign path syntax is explicit. Windows separators are equivalent, but names
/// are compared exactly: no Unicode normalization or case folding guesses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PathReference {
    /// Older catalogs did not retain an encoding; explicit declaration is required.
    Unspecified(Vec<u8>),
    Native(NativePath),
    LegacyUnix(Vec<u8>),
    LegacyWindows(Vec<u16>),
}
impl PathReference {
    pub fn native(path: &Path) -> Self {
        Self::Native(NativePath::from_path(path))
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RelinkScope {
    Prefix {
        from: PathReference,
        destinations: Vec<NativePath>,
    },
    Asset {
        asset_id: String,
        destinations: Vec<NativePath>,
    },
    Volume {
        logical_volume: String,
        mount: storage_volume::MountedVolume,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchStatus {
    Matched,
    Missing,
    Mismatch,
    Ambiguous,
    Unavailable,
    Excluded,
}
impl MatchStatus {
    fn name(&self) -> &'static str {
        match self {
            Self::Matched => "matched",
            Self::Missing => "missing",
            Self::Mismatch => "mismatch",
            Self::Ambiguous => "ambiguous",
            Self::Unavailable => "unavailable",
            Self::Excluded => "excluded",
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub path: NativePath,
    pub status: MatchStatus,
    pub detail: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelinkItem {
    pub sequence: i64,
    pub asset_id: String,
    pub status: String,
    pub detail: String,
    pub original: PathReference,
    pub candidates: Vec<Candidate>,
    pub destination: Option<NativePath>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelinkSource {
    pub source_id: i64,
    pub status: String,
    pub detail: String,
    pub original: PathReference,
    pub candidates: Vec<Candidate>,
    pub destination: Option<NativePath>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelinkPlan {
    pub id: String,
    pub state: String,
    pub scanned_through: i64,
    pub high_water: i64,
    pub total: i64,
    pub matched: i64,
    pub excluded: i64,
    pub unresolved: i64,
    pub unresolved_sources: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageStatus {
    pub asset_id: String,
    pub state: String,
    pub current: PathReference,
    pub candidate: Option<NativePath>,
    pub logical_volume: Option<String>,
    pub detail: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Binding {
    reference: PathReference,
    native_path: NativePath,
    volume_id: Option<String>,
    relative: Option<NativePath>,
    file_key: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BindingDraft {
    identity: Option<PersistentVolumeId>,
    relative: Option<NativePath>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Evidence {
    hash: String,
    length: u64,
    modified_ns: u128,
    object: (u64, u64),
}
impl Evidence {
    fn key(&self) -> String {
        format!("{}:{}", self.object.0, self.object.1)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ItemData {
    old_location: Vec<u8>,
    old_display: String,
    old_binding: Option<Binding>,
    reference: PathReference,
    fingerprint: Option<String>,
    generation: i64,
    metadata_revision: i64,
    candidates: Vec<Candidate>,
    destination: Option<NativePath>,
    evidence: Option<Evidence>,
    binding: Option<BindingDraft>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SourceTag {
    native: Option<NativePath>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SourceData {
    embedded: bool,
    old_native: Option<NativePath>,
    old_tag: Option<SourceTag>,
    old_locator: Vec<u8>,
    old_display: String,
    old_availability: String,
    observation: Option<i64>,
    candidates: Vec<Candidate>,
    destination: Option<NativePath>,
    evidence: Option<Evidence>,
}
fn json<T: Serialize>(v: &T) -> Result<String> {
    Ok(serde_json::to_string(v)?)
}
fn epoch(db: &Connection) -> Result<i64> {
    Ok(
        db.query_row("SELECT revision FROM storage_epoch WHERE id=1", [], |r| {
            r.get(0)
        })?,
    )
}
fn get_binding(db: &Connection, asset: &str) -> Result<Option<Binding>> {
    let row=db.query_row("SELECT reference,native_path,volume_id,relative,file_key FROM storage_bindings WHERE asset_id=?",[asset],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get(2)?,r.get::<_,Option<String>>(3)?,r.get(4)?))).optional()?;
    row.map(|(reference, native_path, volume_id, relative, file_key)| {
        Ok(Binding {
            reference: serde_json::from_str(&reference)?,
            native_path: serde_json::from_str(&native_path)?,
            volume_id,
            relative: relative.map(|v| serde_json::from_str(&v)).transpose()?,
            file_key,
        })
    })
    .transpose()
}
fn put_binding(db: &Connection, asset: &str, b: &Binding) -> Result<()> {
    if get_binding(db, asset)?.as_ref() == Some(b) {
        return Ok(());
    }
    db.execute("INSERT INTO storage_bindings(asset_id,reference,native_path,volume_id,relative,file_key) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(asset_id) DO UPDATE SET reference=excluded.reference,native_path=excluded.native_path,volume_id=excluded.volume_id,relative=excluded.relative,file_key=excluded.file_key",params![asset,json(&b.reference)?,json(&b.native_path)?,b.volume_id,b.relative.as_ref().map(json).transpose()?,b.file_key])?;
    crate::organization::refresh(db, asset)?;
    Ok(())
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum StorageEncoding {
    Unix,
    Windows,
}
#[derive(Debug, Serialize)]
pub struct EncodingProgress {
    pub declared: usize,
    pub scanned_through: i64,
}
pub(crate) fn encoded_bytes(path: &NativePath) -> Vec<u8> {
    match path {
        NativePath::UnixBytes(v) => v.clone(),
        NativePath::WindowsWide(v) => v.iter().flat_map(|v| v.to_le_bytes()).collect(),
    }
}
fn decode_bytes(bytes: &[u8], encoding: StorageEncoding) -> Result<NativePath> {
    match encoding {
        StorageEncoding::Unix => Ok(NativePath::UnixBytes(bytes.to_vec())),
        StorageEncoding::Windows => {
            ensure!(
                bytes.len().is_multiple_of(2),
                "invalid UTF-16 locator length"
            );
            Ok(NativePath::WindowsWide(
                bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|v| u16::from_le_bytes([v[0], v[1]]))
                    .collect(),
            ))
        }
    }
}
fn get_source_tag(db: &Connection, source: i64) -> Result<Option<SourceTag>> {
    db.query_row(
        "SELECT tag FROM storage_source_locators WHERE source_id=?",
        [source],
        |r| r.get::<_, String>(0),
    )
    .optional()?
    .map(|v| Ok(serde_json::from_str(&v)?))
    .transpose()
}
fn put_source_tag(db: &Connection, source: i64, tag: Option<&SourceTag>) -> Result<()> {
    if get_source_tag(db, source)?.as_ref() == tag {
        return Ok(());
    }
    if let Some(tag) = tag {
        if let Some(path) = &tag.native {
            let bytes: Vec<u8> = db.query_row(
                "SELECT locator FROM metadata_sources WHERE id=?",
                [source],
                |r| r.get(0),
            )?;
            ensure!(
                encoded_bytes(path) == bytes,
                "source locator tag does not match current bytes"
            );
        }
        db.execute("INSERT INTO storage_source_locators VALUES(?1,?2) ON CONFLICT(source_id) DO UPDATE SET tag=excluded.tag",params![source,json(tag)?])?;
    } else {
        db.execute(
            "DELETE FROM storage_source_locators WHERE source_id=?",
            [source],
        )?;
    }
    Ok(())
}
pub(crate) fn record_metadata_path(
    db: &Connection,
    asset: &str,
    kind: &str,
    path: &Path,
) -> Result<()> {
    let source: Option<i64> = db
        .query_row(
            "SELECT id FROM metadata_sources WHERE asset_id=?1 AND kind=?2 AND locator=?3",
            params![asset, kind, location_bytes(path)],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(source) = source {
        let native = NativePath::from_path(path);
        if let Some(SourceTag { native: Some(old) }) = get_source_tag(db, source)? {
            ensure!(
                old == native,
                "source locator encoding differs; explicit review required"
            );
        }
        put_source_tag(
            db,
            source,
            Some(&SourceTag {
                native: Some(native),
            }),
        )?;
    }
    Ok(())
}
fn source_native(
    db: &Connection,
    source: i64,
    item: &ItemData,
    bytes: &[u8],
) -> Result<Option<NativePath>> {
    if let Some(tag) = get_source_tag(db, source)? {
        if let Some(native) = &tag.native {
            ensure!(
                encoded_bytes(native) == bytes,
                "source locator tag differs from stored bytes"
            );
        }
        return Ok(tag.native);
    }
    // Only legacy untagged sources inherit the asset's original encoding. Every
    // apply seals even an unknown source tag before changing that asset binding.
    item.old_binding
        .as_ref()
        .map(|b| {
            decode_bytes(
                bytes,
                match b.native_path {
                    NativePath::UnixBytes(_) => StorageEncoding::Unix,
                    NativePath::WindowsWide(_) => StorageEncoding::Windows,
                },
            )
        })
        .transpose()
}
fn volume_id(
    db: &Connection,
    identity: &Option<PersistentVolumeId>,
    fallback: Option<&str>,
) -> Result<String> {
    let encoded = identity.as_ref().map(json).transpose()?;
    if let Some(ref id) = encoded
        && let Some(existing) = db
            .query_row(
                "SELECT id FROM storage_volumes WHERE identity=?",
                [id],
                |r| r.get(0),
            )
            .optional()?
    {
        return Ok(existing);
    }
    if identity.is_none()
        && let Some(fallback) = fallback
    {
        let stored: Option<String> = db.query_row(
            "SELECT identity FROM storage_volumes WHERE id=?",
            [fallback],
            |r| r.get(0),
        )?;
        if stored.is_none() {
            return Ok(fallback.into());
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    db.execute(
        "INSERT INTO storage_volumes VALUES(?1,?2)",
        params![id, encoded],
    )?;
    Ok(id)
}
/// Record a reviewed locator inside the caller's asset-registration transaction.
/// This validates encoding and path structure without probing the filesystem.
pub(crate) fn record_storage_path(db: &Connection, asset: &str, path: &NativePath) -> Result<()> {
    components(&PathReference::Native(path.clone()))?;
    let location: Vec<u8> =
        db.query_row("SELECT location FROM assets WHERE id=?", [asset], |r| {
            r.get(0)
        })?;
    ensure!(
        location == encoded_bytes(path),
        "declared native locator differs from stored bytes"
    );
    if let Some(old) = get_binding(db, asset)? {
        ensure!(
            old.native_path == *path,
            "existing locator encoding differs; explicit review required"
        );
    } else {
        put_binding(
            db,
            asset,
            &Binding {
                reference: PathReference::Native(path.clone()),
                native_path: path.clone(),
                volume_id: None,
                relative: None,
                file_key: None,
            },
        )?;
    }
    Ok(())
}

impl Catalog {
    /// Record explicit encoding without filesystem queries, including unavailable
    /// volumes. Call after reserve for every native import. Foreign declarations
    /// must match the stored bytes exactly and remain unavailable until relinked.
    pub fn record_storage_path(&mut self, asset: &str, path: &NativePath) -> Result<()> {
        components(&PathReference::Native(path.clone()))?;
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Background)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        record_storage_path(&tx, asset, path)?;
        tx.commit()?;
        drop(_write);
        Ok(())
    }
    /// Explicit migration declaration, bounded and restartable. Existing tags are
    /// never overwritten; malformed paths fail the batch without partial changes.
    pub fn declare_storage_encoding(
        &mut self,
        encoding: StorageEncoding,
        after: i64,
        limit: usize,
    ) -> Result<EncodingProgress> {
        ensure!((1..=1000).contains(&limit), "batch limit must be 1..1000");
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let rows=tx.prepare("SELECT a.sequence,a.id,a.location FROM assets a LEFT JOIN storage_bindings b ON b.asset_id=a.id WHERE a.sequence>?1 AND b.asset_id IS NULL ORDER BY a.sequence LIMIT ?2")?.query_map(params![after,limit as i64],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,Vec<u8>>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for (_, asset, bytes) in &rows {
            let path = decode_bytes(bytes, encoding)?;
            components(&PathReference::Native(path.clone()))?;
            put_binding(
                &tx,
                asset,
                &Binding {
                    reference: PathReference::Native(path.clone()),
                    native_path: path,
                    volume_id: None,
                    relative: None,
                    file_key: None,
                },
            )?;
        }
        let progress = EncodingProgress {
            declared: rows.len(),
            scanned_through: rows.last().map(|v| v.0).unwrap_or(after),
        };
        tx.commit()?;
        drop(_write);
        Ok(progress)
    }
    /// Caller supplies the volume observation from the same verified import. This
    /// records provenance, not permission to match a future file by name alone.
    pub fn bind_storage(&mut self, asset: &str, observation: &VolumeLocation) -> Result<()> {
        ensure!(
            observation.state == LocationState::Available,
            "cannot bind unavailable original"
        );
        let path = observation.requested_path.to_path()?;
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Background)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let location: Vec<u8> =
            tx.query_row("SELECT location FROM assets WHERE id=?", [asset], |r| {
                r.get(0)
            })?;
        ensure!(
            location == location_bytes(&path),
            "binding observation does not identify stored path"
        );
        let old = get_binding(&tx, asset)?;
        let identity = observation
            .volume
            .as_ref()
            .and_then(|v| v.persistent_identity.clone());
        if identity.is_none()
            && let Some(old_id) = old.as_ref().and_then(|b| b.volume_id.as_ref())
        {
            let known: Option<String> = tx.query_row(
                "SELECT identity FROM storage_volumes WHERE id=?",
                [old_id],
                |r| r.get(0),
            )?;
            if known.is_some() {
                tx.commit()?;
                drop(_write);
                return Ok(());
            }
        }
        let id = volume_id(
            &tx,
            &identity,
            old.as_ref().and_then(|v| v.volume_id.as_deref()),
        )?;
        let file = open_regular(&path)?;
        let key = object_key(&file)?;
        put_binding(
            &tx,
            asset,
            &Binding {
                reference: PathReference::native(&path),
                native_path: NativePath::from_path(&path),
                relative: observation.relative_in_volume.clone().or_else(|| {
                    old.as_ref()
                        .filter(|b| b.volume_id.as_deref() == Some(&id))
                        .and_then(|b| b.relative.clone())
                }),
                volume_id: Some(id),
                file_key: Some(format!("{}:{}", key.0, key.1)),
            },
        )?;
        tx.commit()?;
        drop(_write);
        Ok(())
    }
    /// Explicit migration input; this never converts or guesses the host syntax.
    pub fn set_legacy_storage_path(&mut self, asset: &str, reference: PathReference) -> Result<()> {
        ensure!(
            !matches!(reference, PathReference::Native(_)),
            "use bind_storage for native locations"
        );
        components(&reference)?;
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.query_row("SELECT id FROM assets WHERE id=?", [asset], |r| {
            r.get::<_, String>(0)
        })?;
        let native_path = get_binding(&tx, asset)?
            .context("declare current locator encoding before assigning a foreign reference")?
            .native_path;
        put_binding(
            &tx,
            asset,
            &Binding {
                reference,
                native_path,
                volume_id: None,
                relative: None,
                file_key: None,
            },
        )?;
        tx.commit()?;
        drop(_write);
        Ok(())
    }
    pub fn storage_status(&self, asset: &str, snapshot: &MountSnapshot) -> Result<StorageStatus> {
        let location: Vec<u8> =
            self.db
                .query_row("SELECT location FROM assets WHERE id=?", [asset], |r| {
                    r.get(0)
                })?;
        let b = get_binding(&self.db, asset)?;
        let reference = match b.as_ref() {
            Some(b) => b.reference.clone(),
            None => PathReference::Unspecified(location.clone()),
        };
        let mut result = StorageStatus {
            asset_id: asset.into(),
            state: "unregistered".into(),
            current: reference,
            candidate: None,
            logical_volume: b.as_ref().and_then(|b| b.volume_id.clone()),
            detail: "No stable volume binding; explicit relink remains available".into(),
        };
        if let Some(b) = b
            && let Some(id) = b.volume_id
        {
            let encoded: Option<String> = self.db.query_row(
                "SELECT identity FROM storage_volumes WHERE id=?",
                [&id],
                |r| r.get(0),
            )?;
            let logical = LogicalVolume {
                id,
                persistent_identity: encoded.map(|v| serde_json::from_str(&v)).transpose()?,
            };
            match storage_volume::match_mounts(&logical, snapshot) {
                MountMatch::Unique(mount) => {
                    result.state = "online_unverified".into();
                    result.detail =
                        "Volume found; content verification required before reconnect".into();
                    if let Some(relative) = b.relative {
                        match storage_volume::candidate_path(&mount, &relative) {
                            Ok(p) => {
                                result.candidate = Some(NativePath::from_path(&p));
                                let observation = storage_volume::locate(&p);
                                result.state = match observation.state {
                                    LocationState::Available => "online_unverified",
                                    LocationState::MissingPath => "missing",
                                    _ => "unavailable",
                                }
                                .into();
                            }
                            Err(e) => {
                                result.state = "unavailable".into();
                                result.detail = e.to_string();
                            }
                        }
                    }
                }
                MountMatch::Offline => result.state = "offline".into(),
                MountMatch::Ambiguous(_) => result.state = "ambiguous".into(),
                MountMatch::Indeterminate => result.state = "indeterminate".into(),
                MountMatch::IdentityUnavailable => result.state = "identity_unavailable".into(),
            }
        }
        Ok(result)
    }
    /// None means no binding exists. Known ambiguity/mismatch is an error so import
    /// cannot turn an offline original into a second catalog identity.
    pub fn reconnect_storage_asset(
        &mut self,
        path: &Path,
        observation: &VolumeLocation,
        fingerprint: &str,
        snapshot: &MountSnapshot,
    ) -> Result<Option<String>> {
        ensure!(
            observation.requested_path == NativePath::from_path(path),
            "reconnect observation path mismatch"
        );
        if observation.state != LocationState::Available {
            return Ok(None);
        }
        let observed_identity = observation
            .volume
            .as_ref()
            .and_then(|v| v.persistent_identity.as_ref())
            .map(json)
            .transpose()?;
        let existing:Option<(Option<String>,Option<String>)>=self.db.query_row("SELECT a.fingerprint,v.identity FROM assets a JOIN storage_bindings b ON b.asset_id=a.id JOIN storage_volumes v ON v.id=b.volume_id WHERE a.location=?",[location_bytes(path)],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        if let Some((expected, Some(known_identity))) = existing {
            ensure!(
                observed_identity.as_deref() == Some(&known_identity)
                    || expected.as_deref() == Some(fingerprint),
                "different or unidentified volume at an existing original path has different content; explicit review required"
            );
        }
        let Some(identity) = observation
            .volume
            .as_ref()
            .and_then(|v| v.persistent_identity.as_ref())
        else {
            return Ok(None);
        };
        let Some(relative) = &observation.relative_in_volume else {
            return Ok(None);
        };
        let mut stmt=self.db.prepare("SELECT a.id,a.fingerprint,a.location FROM storage_volumes v JOIN storage_bindings b ON b.volume_id=v.id JOIN assets a ON a.id=b.asset_id WHERE v.identity=?1 AND b.relative=?2 LIMIT 2")?;
        let rows = stmt
            .query_map(params![json(identity)?, json(relative)?], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        ensure!(
            rows.len() < 2,
            "ambiguous volume-relative original; explicit relink review required"
        );
        let Some((asset, expected, old)) = rows.into_iter().next() else {
            return Ok(None);
        };
        if old == location_bytes(path) {
            return Ok(Some(asset));
        }
        ensure!(
            !observation
                .issues
                .iter()
                .chain(observation.volume.iter().flat_map(|v| v.issues.iter()))
                .any(|issue| matches!(
                    issue.kind,
                    storage_volume::IssueKind::IdentityAmbiguous
                        | storage_volume::IssueKind::ObservationChanged
                )),
            "ambiguous or changing volume observation; explicit review required"
        );
        let logical = LogicalVolume {
            id: String::new(),
            persistent_identity: Some(identity.clone()),
        };
        ensure!(
            matches!(
                storage_volume::match_mounts(&logical, snapshot),
                MountMatch::Unique(_)
            ),
            "known volume has ambiguous or incomplete mount evidence; explicit relink required"
        );
        ensure!(
            expected.as_deref() == Some(fingerprint),
            "known volume-relative original has different content; review required"
        );
        let plan = self.begin_relink(RelinkScope::Asset {
            asset_id: asset.clone(),
            destinations: vec![NativePath::from_path(path)],
        })?;
        self.prepare_relink_batch(&plan.id, 1)?;
        // Missing sidecars remain visible for explicit review; automatic reconnect
        // never discards their locators or makes an unverified name association.
        self.apply_relink(&plan.id)?;
        Ok(Some(asset))
    }
    pub fn begin_relink(&mut self, mut request: RelinkScope) -> Result<RelinkPlan> {
        validate_request(&request)?;
        if let RelinkScope::Prefix {
            from: PathReference::Native(path),
            ..
        } = &mut request
            && let Ok(native) = path.to_path()
        {
            *path = NativePath::from_path(&crate::prospective_directory(&native)?);
        }
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let RelinkScope::Volume {
            logical_volume,
            mount,
        } = &request
        {
            let identity: Option<String> = tx.query_row(
                "SELECT identity FROM storage_volumes WHERE id=?",
                [logical_volume],
                |r| r.get(0),
            )?;
            ensure!(
                identity.is_some()
                    && identity == mount.persistent_identity.as_ref().map(json).transpose()?,
                "volume mapping has no matching persistent identity"
            );
        }
        if let RelinkScope::Asset { asset_id, .. } = &request {
            ensure!(
                tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM assets WHERE id=?)",
                    [asset_id],
                    |r| r.get::<_, bool>(0)
                )?,
                "asset does not exist"
            );
        }
        let high: i64 = tx.query_row("SELECT COALESCE(MAX(sequence),0) FROM assets", [], |r| {
            r.get(0)
        })?;
        let id = uuid::Uuid::new_v4().to_string();
        tx.execute("INSERT INTO storage_plans(id,request,epoch,high_water,state) VALUES(?1,?2,?3,?4,'preparing')",params![id,json(&request)?,epoch(&tx)?,high])?;
        tx.commit()?;
        drop(_write);
        self.relink_plan(&id)
    }
    /// Exceptions are disk-backed and must be fixed before preparing any rows.
    /// Empty candidates explicitly leave this asset/source at its old locator.
    pub fn set_relink_candidates(
        &mut self,
        plan: &str,
        asset: &str,
        candidates: Vec<NativePath>,
    ) -> Result<()> {
        self.set_storage_exception(plan, "asset", asset, candidates)
    }
    pub fn set_relink_source_candidates(
        &mut self,
        plan: &str,
        source: i64,
        candidates: Vec<NativePath>,
    ) -> Result<()> {
        let kind: String = self.db.query_row(
            "SELECT kind FROM metadata_sources WHERE id=?",
            [source],
            |r| r.get(0),
        )?;
        ensure!(
            kind == "sidecar",
            "embedded metadata must follow its original; only sidecar exceptions are allowed"
        );
        self.set_storage_exception(plan, "source", &source.to_string(), candidates)
    }
    fn set_storage_exception(
        &mut self,
        plan: &str,
        kind: &str,
        entity: &str,
        candidates: Vec<NativePath>,
    ) -> Result<()> {
        validate_candidates(&candidates)?;
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let (state, cursor): (String, i64) = tx.query_row(
            "SELECT state,cursor FROM storage_plans WHERE id=?",
            [plan],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        ensure!(
            state == "preparing" && cursor == 0,
            "exceptions must precede preparation; create a new plan"
        );
        let exists: bool = if kind == "asset" {
            tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM assets WHERE id=?)",
                [entity],
                |r| r.get(0),
            )?
        } else {
            tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM metadata_sources WHERE id=?)",
                [entity],
                |r| r.get(0),
            )?
        };
        ensure!(exists, "exception entity does not exist");
        tx.execute("INSERT INTO storage_exceptions VALUES(?1,?2,?3,?4) ON CONFLICT(plan,kind,entity) DO UPDATE SET candidates=excluded.candidates",params![plan,kind,entity,json(&candidates)?])?;
        tx.commit()?;
        drop(_write);
        Ok(())
    }
    pub fn prepare_relink_batch(&mut self, plan: &str, limit: usize) -> Result<RelinkPlan> {
        ensure!((1..=1000).contains(&limit), "batch limit must be 1..1000");
        let catalog_root = self.root.clone();
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let (request, snapshot, cursor, high, state): (String, i64, i64, i64, String) = tx
            .query_row(
                "SELECT request,epoch,cursor,high_water,state FROM storage_plans WHERE id=?",
                [plan],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )?;
        ensure!(state == "preparing", "plan is not preparing");
        ensure!(
            snapshot == epoch(&tx)?,
            "stale relink plan: catalog locations or sources changed"
        );
        let request: RelinkScope = serde_json::from_str(&request)?;
        let (filter, value) = match &request {
            RelinkScope::Asset { asset_id, .. } => ("AND a.id=?4", asset_id.as_str()),
            RelinkScope::Volume { logical_volume, .. } => (
                "AND a.id IN (SELECT asset_id FROM storage_bindings WHERE volume_id=?4)",
                logical_volume.as_str(),
            ),
            _ => ("AND ?4 IS NOT NULL", ""),
        };
        let query = format!(
            "SELECT a.sequence,a.id FROM assets a WHERE a.sequence>?1 AND a.sequence<=?2 {filter} ORDER BY a.sequence LIMIT ?3"
        );
        let rows = tx
            .prepare(&query)?
            .query_map(params![cursor, high, limit as i64, value], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (sequence, asset) in &rows {
            let mut data = load_item(&tx, asset)?;
            let override_paths = exception(&tx, plan, "asset", asset)?;
            let paths = match &override_paths {
                Some(paths) => Some(paths.clone()),
                None => {
                    mapped_candidates(&request, asset, &data.reference, data.old_binding.as_ref())?
                }
            };
            let Some(paths) = paths else {
                continue;
            };
            let (status, detail, candidates, selected) =
                if override_paths.as_ref().is_some_and(Vec::is_empty) {
                    (
                        MatchStatus::Excluded,
                        "Explicitly excluded".into(),
                        vec![],
                        None,
                    )
                } else {
                    evaluate(&paths, data.fingerprint.as_deref(), &catalog_root)
                };
            data.candidates = candidates;
            if let Some((path, evidence)) = selected {
                data.binding = Some(binding_draft(&path.to_path()?));
                data.destination = Some(path);
                data.evidence = Some(evidence);
            }
            let destination = data
                .destination
                .as_ref()
                .map(|p| p.to_path().map(|p| location_bytes(&p)))
                .transpose()?;
            tx.execute(
                "INSERT INTO storage_items VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    plan,
                    sequence,
                    asset,
                    status.name(),
                    detail,
                    destination,
                    data.evidence.as_ref().map(Evidence::key),
                    json(&data)?
                ],
            )?;
            if status != MatchStatus::Excluded {
                prepare_sources(&tx, plan, *sequence, asset, &request, &data, &catalog_root)?;
            }
        }
        let finished = rows.len() < limit
            || matches!(request, RelinkScope::Asset { .. })
            || rows.last().is_some_and(|(n, _)| *n == high);
        let next = if finished {
            high
        } else {
            rows.last().map(|r| r.0).unwrap_or(high)
        };
        tx.execute(
            "UPDATE storage_plans SET cursor=?2,state=?3 WHERE id=?1",
            params![plan, next, if finished { "ready" } else { "preparing" }],
        )?;
        if finished {
            mark_collisions(&tx, plan)?;
        }
        tx.commit()?;
        drop(_write);
        self.relink_plan(plan)
    }
    /// Restart discovery, ordered by opaque operation ID with a bounded cursor.
    pub fn relink_plans(&self, after: &str, limit: usize) -> Result<Vec<RelinkPlan>> {
        ensure!((1..=1000).contains(&limit), "page limit must be 1..1000");
        let ids = self
            .db
            .prepare("SELECT id FROM storage_plans WHERE id>?1 ORDER BY id LIMIT ?2")?
            .query_map(params![after, limit as i64], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.iter().map(|id| self.relink_plan(id)).collect()
    }
    pub fn relink_plan(&self, id: &str) -> Result<RelinkPlan> {
        let (state, cursor, high) = self.db.query_row(
            "SELECT state,cursor,high_water FROM storage_plans WHERE id=?",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let (total,matched,excluded,unresolved)=self.db.query_row("SELECT COUNT(*),COALESCE(SUM(status='matched'),0),COALESCE(SUM(status='excluded'),0),COALESCE(SUM(status NOT IN ('matched','excluded')),0) FROM storage_items WHERE plan=?",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        let unresolved_sources=self.db.query_row("SELECT COUNT(*) FROM storage_source_items s JOIN storage_items i ON i.plan=s.plan AND i.sequence=s.sequence WHERE s.plan=? AND i.status!='excluded' AND s.status NOT IN ('matched','excluded')",[id],|r|r.get(0))?;
        Ok(RelinkPlan {
            id: id.into(),
            state,
            scanned_through: cursor,
            high_water: high,
            total,
            matched,
            excluded,
            unresolved,
            unresolved_sources,
        })
    }
    pub fn relink_items(&self, plan: &str, after: i64, limit: usize) -> Result<Vec<RelinkItem>> {
        ensure!((1..=1000).contains(&limit), "page limit must be 1..1000");
        let mut stmt=self.db.prepare("SELECT sequence,asset_id,status,detail,data FROM storage_items WHERE plan=?1 AND sequence>?2 ORDER BY sequence LIMIT ?3")?;
        let mut out = Vec::new();
        for row in stmt.query_map(params![plan, after, limit as i64], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get::<_, String>(4)?,
            ))
        })? {
            let (sequence, asset_id, status, detail, data) = row?;
            let data: ItemData = serde_json::from_str(&data)?;
            out.push(RelinkItem {
                sequence,
                asset_id,
                status,
                detail,
                original: data.reference,
                candidates: data.candidates,
                destination: data.destination,
            });
        }
        Ok(out)
    }
    pub fn relink_sources(
        &self,
        plan: &str,
        sequence: i64,
        after: i64,
        limit: usize,
    ) -> Result<Vec<RelinkSource>> {
        ensure!((1..=1000).contains(&limit), "page limit must be 1..1000");
        let mut stmt=self.db.prepare("SELECT source_id,status,detail,data FROM storage_source_items WHERE plan=?1 AND sequence=?2 AND source_id>?3 ORDER BY source_id LIMIT ?4")?;
        let mut out = Vec::new();
        for row in stmt.query_map(params![plan, sequence, after, limit as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, String>(3)?))
        })? {
            let (source_id, status, detail, data) = row?;
            let data: SourceData = serde_json::from_str(&data)?;
            out.push(RelinkSource {
                source_id,
                status,
                detail,
                original: data
                    .old_native
                    .map(PathReference::Native)
                    .unwrap_or(PathReference::Unspecified(data.old_locator)),
                candidates: data.candidates,
                destination: data.destination,
            });
        }
        Ok(out)
    }
    /// Explicit exclusions after preview are allowed only before apply. The
    /// unresolved evidence remains on disk; an excluded source keeps its locator.
    pub fn exclude_relink_item(&mut self, plan: &str, sequence: i64) -> Result<()> {
        self.exclude_storage(plan, sequence, None)
    }
    pub fn exclude_relink_source(&mut self, plan: &str, source: i64) -> Result<()> {
        let kind: String = self.db.query_row(
            "SELECT kind FROM metadata_sources WHERE id=?",
            [source],
            |r| r.get(0),
        )?;
        ensure!(
            kind == "sidecar",
            "embedded metadata must follow its original; exclude the asset instead"
        );
        self.exclude_storage(plan, 0, Some(source))
    }
    fn exclude_storage(&mut self, plan: &str, sequence: i64, source: Option<i64>) -> Result<()> {
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let state: String =
            tx.query_row("SELECT state FROM storage_plans WHERE id=?", [plan], |r| {
                r.get(0)
            })?;
        ensure!(
            state == "ready",
            "plan must be ready for explicit exclusion"
        );
        let changed = if let Some(source) = source {
            tx.execute("UPDATE storage_source_items SET status='excluded',detail='Explicitly excluded after preview' WHERE plan=?1 AND source_id=?2",params![plan,source])?
        } else {
            tx.execute("UPDATE storage_items SET status='excluded',detail='Explicitly excluded after preview' WHERE plan=?1 AND sequence=?2",params![plan,sequence])?
        };
        ensure!(changed == 1, "relink item not found");
        tx.commit()?;
        drop(_write);
        Ok(())
    }
    pub fn apply_relink(&mut self, plan: &str) -> Result<RelinkPlan> {
        self.apply_relink_with(plan, |_| Ok(()))
    }
    /// Fault seam: callback errors at any boundary roll back every catalog row.
    /// Callbacks may not change catalog data or reenter Catalog methods.
    #[doc(hidden)]
    pub fn apply_relink_with(
        &mut self,
        plan: &str,
        mut boundary: impl FnMut(RelinkBoundary) -> Result<()>,
    ) -> Result<RelinkPlan> {
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let (state, snapshot): (String, i64) = tx.query_row(
            "SELECT state,epoch FROM storage_plans WHERE id=?",
            [plan],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if state == "applied" {
            tx.commit()?;
            drop(_write);
            return self.relink_plan(plan);
        }
        ensure!(
            state == "ready" && snapshot == epoch(&tx)?,
            "plan is incomplete or stale"
        );
        let unresolved:i64=tx.query_row("SELECT (SELECT COUNT(*) FROM storage_items WHERE plan=?1 AND status NOT IN ('matched','excluded'))+(SELECT COUNT(*) FROM storage_source_items s JOIN storage_items i ON i.plan=s.plan AND i.sequence=s.sequence WHERE s.plan=?1 AND i.status!='excluded' AND s.status NOT IN ('matched','excluded'))",[plan],|r|r.get(0))?;
        ensure!(
            unresolved == 0,
            "relink requires review of unmatched assets or metadata sources"
        );
        validate_collisions(&tx, plan)?;
        // This is intentionally streamed inside one write transaction. It trades
        // writer occupancy for atomic catalog publication, without an O(n) heap.
        visit_items(&tx, plan, |sequence, asset, data| {
            let current = load_item(&tx, asset)?;
            ensure!(
                current.old_location == data.old_location
                    && current.fingerprint == data.fingerprint
                    && current.generation == data.generation
                    && current.metadata_revision == data.metadata_revision
                    && current.old_binding == data.old_binding,
                "asset changed after planning"
            );
            verify_destination(data.destination.as_ref(), data.evidence.as_ref())?;
            visit_sources(&tx, plan, sequence, |source, status, data| {
                check_source_snapshot(&tx, source, data, false, false)?;
                if status == "matched" && !data.embedded {
                    verify_destination(data.destination.as_ref(), data.evidence.as_ref())?;
                }
                Ok(())
            })?;
            boundary(RelinkBoundary::Verified(sequence))?;
            Ok(())
        })?;
        boundary(RelinkBoundary::BeforeMutation)?;
        visit_items(&tx, plan, |_, asset, _| {
            record_predecessor(&tx, plan, asset)
        })?;
        // Internal BLOB keys cannot encode an absolute path on supported platforms.
        visit_items(&tx, plan, |_, asset, _| {
            tx.execute(
                "UPDATE assets SET location=?2 WHERE id=?1",
                params![asset, temporary_location(plan, "asset", asset)],
            )?;
            Ok(())
        })?;
        visit_items(&tx, plan, |sequence, asset, data| {
            let path = data
                .destination
                .as_ref()
                .context("matched destination missing")?
                .to_path()?;
            let changed = location_bytes(&path) != data.old_location;
            tx.execute("UPDATE assets SET location=?2,path_display=?3,render_generation=render_generation+?4 WHERE id=?1",params![asset,location_bytes(&path),path.to_string_lossy(),i64::from(changed)])?;
            let draft = data
                .binding
                .as_ref()
                .context("matched volume evidence missing")?;
            let volume = volume_id(
                &tx,
                &draft.identity,
                data.old_binding
                    .as_ref()
                    .and_then(|b| b.volume_id.as_deref()),
            )?;
            put_binding(
                &tx,
                asset,
                &Binding {
                    reference: PathReference::native(&path),
                    native_path: NativePath::from_path(&path),
                    volume_id: Some(volume),
                    relative: draft.relative.clone(),
                    file_key: data.evidence.as_ref().map(Evidence::key),
                },
            )?;
            visit_sources(&tx, plan, sequence, |source, _, data| {
                put_source_tag(
                    &tx,
                    source,
                    Some(&SourceTag {
                        native: data.old_native.clone(),
                    }),
                )
            })?;
            // Move every source to temporary keys first so source swaps cannot
            // violate (asset,kind,locator) halfway through this transaction.
            visit_sources(&tx, plan, sequence, |source, status, _| {
                if status == "matched" {
                    tx.execute(
                        "UPDATE metadata_sources SET locator=?2 WHERE id=?1",
                        params![
                            source,
                            temporary_location(plan, "source", &source.to_string())
                        ],
                    )?;
                }
                Ok(())
            })?;
            visit_sources(&tx, plan, sequence, |source, status, data| {
                if status == "matched" {
                    let p = data
                        .destination
                        .as_ref()
                        .context("source destination missing")?
                        .to_path()?;
                    tx.execute("UPDATE metadata_sources SET locator=?2,display=?3,availability='available' WHERE id=?1",params![source,location_bytes(&p),p.to_string_lossy()])?;
                    put_source_tag(
                        &tx,
                        source,
                        Some(&SourceTag {
                            native: data.destination.clone(),
                        }),
                    )?;
                    publish_source_state(&tx, source)?;
                }
                Ok(())
            })?;
            if changed || sources_changed(&tx, plan, sequence)? {
                advance_metadata(&tx, asset, "storage_relink", plan)?;
            }
            crate::organization::refresh(&tx, asset)?;
            record_applied(&tx, plan, asset)?;
            boundary(RelinkBoundary::Updated(sequence))?;
            Ok(())
        })?;
        // Recheck all bytes and object identities immediately before commit;
        // no database/filesystem atomicity is claimed after this observation.
        visit_items(&tx, plan, |sequence, _, data| {
            verify_destination(data.destination.as_ref(), data.evidence.as_ref())?;
            visit_sources(&tx, plan, sequence, |_, status, data| {
                if status == "matched" && !data.embedded {
                    verify_destination(data.destination.as_ref(), data.evidence.as_ref())?;
                }
                Ok(())
            })?;
            Ok(())
        })?;
        crate::catalog_images::step_refresh(&tx, 32)?;
        tx.execute(
            "UPDATE storage_plans SET state='applied',applied_epoch=?2 WHERE id=?1",
            params![plan, epoch(&tx)?],
        )?;
        boundary(RelinkBoundary::BeforeCommit)?;
        tx.commit()?;
        drop(_write);
        self.relink_plan(plan)
    }
    pub fn undo_relink(&mut self, plan: &str) -> Result<RelinkPlan> {
        self.undo_relink_with(plan, |_| Ok(()))
    }
    #[doc(hidden)]
    pub fn undo_relink_with(
        &mut self,
        plan: &str,
        mut boundary: impl FnMut(RelinkBoundary) -> Result<()>,
    ) -> Result<RelinkPlan> {
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let state: String =
            tx.query_row("SELECT state FROM storage_plans WHERE id=?", [plan], |r| {
                r.get(0)
            })?;
        if state == "undone" {
            tx.commit()?;
            drop(_write);
            return self.relink_plan(plan);
        }
        ensure!(state == "applied", "only an applied relink can be undone");
        // Undo restores references even if originals are still offline. It never
        // writes/moves files, resurrects old metadata values, or decrements versions.
        visit_items(&tx, plan, |sequence, asset, data| {
            let head: Option<String> = tx
                .query_row(
                    "SELECT plan FROM storage_heads WHERE asset_id=?",
                    [asset],
                    |r| r.get(0),
                )
                .optional()?;
            let after: String = tx.query_row(
                "SELECT after_state FROM storage_applied_items WHERE plan=?1 AND asset_id=?2",
                params![plan, asset],
                |r| r.get(0),
            )?;
            ensure!(
                head.as_deref() == Some(plan)
                    && serde_json::from_str::<AppliedState>(&after)? == state_of(&tx, asset)?,
                "undo is stale: affected asset, binding, edit, or relink lineage changed"
            );
            let collision:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM assets a WHERE a.location=?1 AND a.id!=?2 AND NOT EXISTS(SELECT 1 FROM storage_items i WHERE i.plan=?3 AND i.asset_id=a.id AND i.status='matched'))",params![data.old_location,asset,plan],|r|r.get(0))?;
            ensure!(
                !collision,
                "old destination belongs to another asset; undo refused"
            );
            let expected = data
                .destination
                .as_ref()
                .context("missing applied destination")?
                .to_path()?;
            let current: Vec<u8> =
                tx.query_row("SELECT location FROM assets WHERE id=?", [asset], |r| {
                    r.get(0)
                })?;
            ensure!(
                current == location_bytes(&expected),
                "applied location changed"
            );
            visit_sources(&tx, plan, sequence, |source, status, data| {
                check_source_snapshot(&tx, source, data, true, status == "matched")?;
                Ok(())
            })?;
            boundary(RelinkBoundary::Verified(sequence))?;
            Ok(())
        })?;
        boundary(RelinkBoundary::BeforeMutation)?;
        visit_items(&tx, plan, |_, asset, _| {
            tx.execute(
                "UPDATE assets SET location=?2 WHERE id=?1",
                params![asset, temporary_location(plan, "undo", asset)],
            )?;
            Ok(())
        })?;
        visit_items(&tx, plan, |sequence, asset, data| {
            let changed = data
                .destination
                .as_ref()
                .map(|p| p.to_path().map(|p| location_bytes(&p)))
                .transpose()?
                .as_ref()
                != Some(&data.old_location);
            tx.execute("UPDATE assets SET location=?2,path_display=?3,render_generation=render_generation+?4 WHERE id=?1",params![asset,data.old_location,data.old_display,i64::from(changed)])?;
            if let Some(binding) = &data.old_binding {
                put_binding(&tx, asset, binding)?;
            } else {
                tx.execute("DELETE FROM storage_bindings WHERE asset_id=?", [asset])?;
            }
            visit_sources(&tx, plan, sequence, |source, status, _| {
                if status == "matched" {
                    tx.execute(
                        "UPDATE metadata_sources SET locator=?2 WHERE id=?1",
                        params![
                            source,
                            temporary_location(plan, "undo-source", &source.to_string())
                        ],
                    )?;
                }
                Ok(())
            })?;
            visit_sources(&tx, plan, sequence, |source, status, data| {
                if status == "matched" {
                    tx.execute("UPDATE metadata_sources SET locator=?2,display=?3,availability=?4 WHERE id=?1",params![source,data.old_locator,data.old_display,data.old_availability])?;
                    publish_source_state(&tx, source)?;
                }
                put_source_tag(&tx, source, data.old_tag.as_ref())?;
                Ok(())
            })?;
            if changed || sources_changed(&tx, plan, sequence)? {
                advance_metadata(&tx, asset, "storage_undo", plan)?;
            }
            crate::organization::refresh(&tx, asset)?;
            restore_predecessor(&tx, plan, asset)?;
            boundary(RelinkBoundary::Updated(sequence))?;
            Ok(())
        })?;
        crate::catalog_images::step_refresh(&tx, 32)?;
        tx.execute("UPDATE storage_plans SET state='undone' WHERE id=?", [plan])?;
        boundary(RelinkBoundary::BeforeCommit)?;
        tx.commit()?;
        drop(_write);
        self.relink_plan(plan)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelinkBoundary {
    Verified(i64),
    BeforeMutation,
    Updated(i64),
    BeforeCommit,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AppliedState {
    location: Vec<u8>,
    fingerprint: Option<String>,
    generation: i64,
    metadata_revision: i64,
    binding: Option<Binding>,
}
fn state_of(db: &Connection, asset: &str) -> Result<AppliedState> {
    let i = load_item(db, asset)?;
    Ok(AppliedState {
        location: i.old_location,
        fingerprint: i.fingerprint,
        generation: i.generation,
        metadata_revision: i.metadata_revision,
        binding: i.old_binding,
    })
}
fn record_predecessor(db: &Connection, plan: &str, asset: &str) -> Result<()> {
    let current = state_of(db, asset)?;
    let previous: Option<String> = db
        .query_row(
            "SELECT plan FROM storage_heads WHERE asset_id=?",
            [asset],
            |r| r.get(0),
        )
        .optional()?;
    let predecessor_valid = if let Some(ref previous) = previous {
        let after: String = db.query_row(
            "SELECT after_state FROM storage_applied_items WHERE plan=?1 AND asset_id=?2",
            params![previous, asset],
            |r| r.get(0),
        )?;
        serde_json::from_str::<AppliedState>(&after)? == current
    } else {
        false
    };
    db.execute(
        "INSERT INTO storage_applied_items VALUES(?1,?2,?3,?4,?5)",
        params![plan, asset, previous, predecessor_valid, json(&current)?],
    )?;
    Ok(())
}
fn record_applied(db: &Connection, plan: &str, asset: &str) -> Result<()> {
    db.execute(
        "UPDATE storage_applied_items SET after_state=?3 WHERE plan=?1 AND asset_id=?2",
        params![plan, asset, json(&state_of(db, asset)?)?],
    )?;
    db.execute("INSERT INTO storage_heads VALUES(?1,?2) ON CONFLICT(asset_id) DO UPDATE SET plan=excluded.plan",params![asset,plan])?;
    Ok(())
}
fn restore_predecessor(db: &Connection, plan: &str, asset: &str) -> Result<()> {
    let(previous,valid):(Option<String>,bool)=db.query_row("SELECT previous,predecessor_valid FROM storage_applied_items WHERE plan=?1 AND asset_id=?2",params![plan,asset],|r|Ok((r.get(0)?,r.get(1)?)))?;
    if let Some(previous) = previous {
        db.execute(
            "UPDATE storage_heads SET plan=?2 WHERE asset_id=?1",
            params![asset, previous],
        )?;
        if valid {
            let previous_state: String = db.query_row(
                "SELECT after_state FROM storage_applied_items WHERE plan=?1 AND asset_id=?2",
                params![previous, asset],
                |r| r.get(0),
            )?;
            let mut expected: AppliedState = serde_json::from_str(&previous_state)?;
            let current = state_of(db, asset)?;
            // Only counters produced by this verified undo may advance a previous
            // operation's CAS. Independent edits cannot regain undo authority.
            expected.generation = current.generation;
            expected.metadata_revision = current.metadata_revision;
            ensure!(
                expected == current,
                "undo lineage does not restore predecessor state"
            );
            db.execute(
                "UPDATE storage_applied_items SET after_state=?3 WHERE plan=?1 AND asset_id=?2",
                params![previous, asset, json(&current)?],
            )?;
        }
    } else {
        db.execute("DELETE FROM storage_heads WHERE asset_id=?", [asset])?;
    }
    Ok(())
}
fn load_item(db: &Connection, asset: &str) -> Result<ItemData> {
    let(location,display,fingerprint,generation,metadata_revision)=db.query_row("SELECT a.location,a.path_display,a.fingerprint,a.render_generation,COALESCE(m.revision,0) FROM assets a LEFT JOIN metadata_assets m ON m.asset_id=a.id WHERE a.id=?",[asset],|r|Ok((r.get::<_,Vec<u8>>(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
    let binding = get_binding(db, asset)?;
    let reference = match binding.as_ref() {
        Some(b) => b.reference.clone(),
        None => PathReference::Unspecified(location.clone()),
    };
    Ok(ItemData {
        old_location: location,
        old_display: display,
        old_binding: binding,
        reference,
        fingerprint,
        generation,
        metadata_revision,
        candidates: vec![],
        destination: None,
        evidence: None,
        binding: None,
    })
}
fn exception(
    db: &Connection,
    plan: &str,
    kind: &str,
    entity: &str,
) -> Result<Option<Vec<NativePath>>> {
    let value: Option<String> = db
        .query_row(
            "SELECT candidates FROM storage_exceptions WHERE plan=?1 AND kind=?2 AND entity=?3",
            params![plan, kind, entity],
            |r| r.get(0),
        )
        .optional()?;
    value.map(|v| Ok(serde_json::from_str(&v)?)).transpose()
}
fn visit_items(
    db: &Connection,
    plan: &str,
    mut visit: impl FnMut(i64, &str, &ItemData) -> Result<()>,
) -> Result<()> {
    let mut stmt=db.prepare("SELECT sequence,asset_id,data FROM storage_items WHERE plan=? AND status='matched' ORDER BY sequence")?;
    for row in stmt.query_map([plan], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (sequence, asset, data) = row?;
        visit(sequence, &asset, &serde_json::from_str(&data)?)?;
    }
    Ok(())
}
fn visit_sources(
    db: &Connection,
    plan: &str,
    sequence: i64,
    mut visit: impl FnMut(i64, &str, &SourceData) -> Result<()>,
) -> Result<()> {
    let mut stmt=db.prepare("SELECT source_id,status,data FROM storage_source_items WHERE plan=?1 AND sequence=?2 ORDER BY source_id")?;
    for row in stmt.query_map(params![plan, sequence], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (source, status, data) = row?;
        visit(source, &status, &serde_json::from_str(&data)?)?;
    }
    Ok(())
}
fn sources_changed(db: &Connection, plan: &str, sequence: i64) -> Result<bool> {
    let mut changed = false;
    visit_sources(db, plan, sequence, |_, status, data| {
        if status == "matched" {
            let path = data
                .destination
                .as_ref()
                .context("source destination missing")?
                .to_path()?;
            changed |=
                location_bytes(&path) != data.old_locator || data.old_availability != "available";
        }
        Ok(())
    })?;
    Ok(changed)
}
// Preserve the legacy master's single relink revision while propagating the final
// source location/state to copies through bounded events. Never publish swap keys.
fn publish_source_state(db: &Connection, source: i64) -> Result<()> {
    db.execute("UPDATE metadata_image_sources SET logical_locator=(SELECT locator FROM metadata_sources WHERE id=?1),association=(SELECT association FROM metadata_sources WHERE id=?1),availability=(SELECT availability FROM metadata_sources WHERE id=?1) WHERE source_id=?1 AND image_id=(SELECT asset_id FROM metadata_sources WHERE id=?1)", [source])?;
    crate::catalog_images::enqueue_source_state(db, source)
}
fn advance_metadata(db: &Connection, asset: &str, action: &str, plan: &str) -> Result<()> {
    db.execute("INSERT INTO metadata_assets(asset_id,revision) VALUES(?1,1) ON CONFLICT(asset_id) DO UPDATE SET revision=revision+1",[asset])?;
    db.execute("INSERT INTO metadata_history(asset_id,revision,action,detail) SELECT asset_id,revision,?2,?3 FROM metadata_assets WHERE asset_id=?1",params![asset,action,json(&serde_json::json!({"plan":plan}))?])?;
    Ok(())
}
fn temporary_location(plan: &str, kind: &str, entity: &str) -> Vec<u8> {
    format!("\0photocatalog-relink/{plan}/{kind}/{entity}").into_bytes()
}
fn check_source_snapshot(
    db: &Connection,
    source: i64,
    data: &SourceData,
    applied: bool,
    moved: bool,
) -> Result<()> {
    let (locator, observation): (Vec<u8>, Option<i64>) = db.query_row(
        "SELECT locator,current_observation FROM metadata_sources WHERE id=?",
        [source],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let expected = if applied && moved {
        location_bytes(
            &data
                .destination
                .as_ref()
                .context("source destination missing")?
                .to_path()?,
        )
    } else {
        data.old_locator.clone()
    };
    let expected_tag = if applied {
        Some(SourceTag {
            native: if moved {
                data.destination.clone()
            } else {
                data.old_native.clone()
            },
        })
    } else {
        data.old_tag.clone()
    };
    ensure!(
        get_source_tag(db, source)? == expected_tag,
        "source locator encoding changed after planning"
    );
    ensure!(
        locator == expected && observation == data.observation,
        "metadata source changed after planning"
    );
    Ok(())
}
fn binding_draft(path: &Path) -> BindingDraft {
    let location = storage_volume::locate(path);
    BindingDraft {
        identity: location.volume.and_then(|v| v.persistent_identity),
        relative: location.relative_in_volume,
    }
}
fn validate_candidates(paths: &[NativePath]) -> Result<()> {
    ensure!(paths.len() <= 32, "at most 32 explicit candidates per item");
    for path in paths {
        let p = path.to_path()?;
        ensure!(p.is_absolute(), "destination must be absolute");
        components(&PathReference::Native(path.clone()))?;
    }
    Ok(())
}
fn validate_request(request: &RelinkScope) -> Result<()> {
    match request {
        RelinkScope::Prefix { from, destinations } => {
            components(from)?;
            ensure!(!destinations.is_empty(), "provide destination roots");
            validate_candidates(destinations)
        }
        RelinkScope::Asset {
            asset_id,
            destinations,
        } => {
            ensure!(
                !asset_id.is_empty() && !destinations.is_empty(),
                "provide asset and destinations"
            );
            validate_candidates(destinations)
        }
        RelinkScope::Volume { mount, .. } => {
            mount.mount_path.to_path()?;
            Ok(())
        }
    }
}
#[derive(Debug, PartialEq, Eq)]
struct Parts {
    windows: bool,
    root: Vec<u16>,
    names: Vec<Vec<u16>>,
}
fn components(reference: &PathReference) -> Result<Parts> {
    let (windows, mut units) = match reference {
        PathReference::Unspecified(_) => anyhow::bail!(
            "locator encoding is unspecified; declare its origin before prefix mapping"
        ),
        PathReference::Native(NativePath::UnixBytes(v)) | PathReference::LegacyUnix(v) => {
            (false, v.iter().map(|v| *v as u16).collect::<Vec<_>>())
        }
        PathReference::Native(NativePath::WindowsWide(v)) | PathReference::LegacyWindows(v) => {
            (true, v.clone())
        }
    };
    // Windows canonical paths commonly use the extended-length spelling. Only
    // the two filesystem forms are aliases; device namespaces remain refused.
    if windows && units.starts_with(&[92, 92, 63, 92]) {
        if units.len() >= 8 && units[4..8] == [85, 78, 67, 92] {
            units = [vec![92, 92], units[8..].to_vec()].concat();
        } else {
            ensure!(
                units.len() >= 7 && units[5] == 58,
                "unsupported Windows device namespace"
            );
            units = units[4..].to_vec();
        }
    }
    ensure!(
        !windows || !units.starts_with(&[92, 92, 46, 92]),
        "Windows device namespace refused"
    );
    ensure!(
        units.len() <= 32768 && !units.contains(&0),
        "NUL or overlong path"
    );
    let separator = |u: u16| u == 47 || (windows && u == 92);
    let (root, start) = if !windows {
        ensure!(
            units.first() == Some(&47),
            "Unix reference must be absolute"
        );
        (vec![47], 1)
    } else if units.len() >= 3
        && ((units[0] >= 65 && units[0] <= 90) || (units[0] >= 97 && units[0] <= 122))
        && units[1] == 58
        && separator(units[2])
    {
        (units[..2].to_vec(), 3)
    } else {
        ensure!(
            units.len() > 4 && separator(units[0]) && separator(units[1]),
            "Windows reference requires drive root or UNC share"
        );
        let server_end = (2..units.len())
            .find(|i| separator(units[*i]))
            .context("UNC share missing")?;
        ensure!(server_end > 2, "UNC server missing");
        let share_start = server_end + 1;
        let share_end = (share_start..units.len())
            .find(|i| separator(units[*i]))
            .unwrap_or(units.len());
        ensure!(share_end > share_start, "UNC share missing");
        let mut root = vec![92, 92];
        root.extend_from_slice(&units[2..server_end]);
        root.push(92);
        root.extend_from_slice(&units[share_start..share_end]);
        (root, (share_end + 1).min(units.len()))
    };
    let mut names = Vec::new();
    for name in units[start..].split(|u| separator(*u)) {
        if name.is_empty() {
            continue;
        }
        ensure!(
            name != [46] && name != [46, 46],
            "dot path components refused"
        );
        names.push(name.to_vec());
    }
    Ok(Parts {
        windows,
        root,
        names,
    })
}
/// Origin-aware folder prefixes for indexed browsing; display strings never form identity.
pub(crate) fn native_folder_chain(path: &NativePath) -> Result<Vec<(NativePath, String)>> {
    let parts = components(&PathReference::Native(path.clone()))?;
    let native = |v: &[u16]| {
        if parts.windows {
            NativePath::WindowsWide(v.to_vec())
        } else {
            NativePath::UnixBytes(v.iter().map(|u| *u as u8).collect())
        }
    };
    let display = |v: &[u16]| {
        if parts.windows {
            String::from_utf16_lossy(v)
        } else {
            String::from_utf8_lossy(&v.iter().map(|u| *u as u8).collect::<Vec<_>>()).into_owned()
        }
    };
    let mut prefix = parts.root.clone();
    if parts.windows {
        prefix.push(92);
    }
    let mut result = vec![(native(&prefix), display(&prefix))];
    for name in parts.names.iter().take(parts.names.len().saturating_sub(1)) {
        let separator = if parts.windows { 92 } else { 47 };
        if prefix.last() != Some(&separator) {
            prefix.push(separator);
        }
        prefix.extend_from_slice(name);
        result.push((native(&prefix), display(name)));
    }
    Ok(result)
}
fn native_component(windows: bool, component: &[u16]) -> Result<std::ffi::OsString> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let bytes = if windows {
            String::from_utf16(component)
                .context("Windows filename cannot be represented losslessly on Unix")?
                .into_bytes()
        } else {
            component.iter().map(|v| *v as u8).collect()
        };
        ensure!(
            !bytes.contains(&47) && !bytes.contains(&0),
            "foreign separator in filename"
        );
        Ok(std::ffi::OsString::from_vec(bytes))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        let units = if windows {
            component.to_vec()
        } else {
            String::from_utf8(component.iter().map(|v| *v as u8).collect())
                .context("Unix filename cannot be represented losslessly on Windows")?
                .encode_utf16()
                .collect()
        };
        ensure!(
            !units.iter().any(|v| matches!(*v, 0 | 47 | 92 | 58)),
            "foreign separator or stream syntax in filename"
        );
        Ok(std::ffi::OsString::from_wide(&units))
    }
}
fn remap(
    reference: &PathReference,
    from: &PathReference,
    destination: &NativePath,
) -> Result<Option<NativePath>> {
    if matches!(reference, PathReference::Unspecified(_)) {
        return Ok(None);
    }
    let source = components(reference)?;
    let prefix = components(from)?;
    if source.windows != prefix.windows
        || source.root != prefix.root
        || !source.names.starts_with(&prefix.names)
    {
        return Ok(None);
    }
    let mut out = destination.to_path()?;
    for component in &source.names[prefix.names.len()..] {
        out.push(native_component(source.windows, component)?);
    }

    Ok(Some(NativePath::from_path(&out)))
}
fn mapped_candidates(
    request: &RelinkScope,
    asset: &str,
    reference: &PathReference,
    binding: Option<&Binding>,
) -> Result<Option<Vec<NativePath>>> {
    match request {
        RelinkScope::Prefix { from, destinations } => {
            let mut result = Vec::new();
            for dest in destinations {
                if let Some(p) = remap(reference, from, dest)? {
                    result.push(p);
                }
            }
            if result.is_empty() {
                Ok(None)
            } else {
                Ok(Some(result))
            }
        }
        RelinkScope::Asset {
            asset_id,
            destinations,
        } => Ok((asset == asset_id).then(|| destinations.clone())),
        RelinkScope::Volume {
            logical_volume,
            mount,
        } => {
            let Some(binding) = binding.filter(|b| b.volume_id.as_deref() == Some(logical_volume))
            else {
                return Ok(None);
            };
            let Some(relative) = &binding.relative else {
                return Ok(Some(vec![]));
            };
            Ok(Some(vec![NativePath::from_path(
                &storage_volume::candidate_path(mount, relative)?,
            )]))
        }
    }
}
fn evaluate(
    paths: &[NativePath],
    expected: Option<&str>,
    catalog_root: &Path,
) -> (
    MatchStatus,
    String,
    Vec<Candidate>,
    Option<(NativePath, Evidence)>,
) {
    let mut candidates = Vec::new();
    let mut matches = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for path in paths {
        if !seen.insert(path.clone()) {
            continue;
        }
        let checked = path.to_path().map_err(anyhow::Error::from).and_then(|p| {
            let evidence = read_evidence(&p)?;
            let canonical = fs::canonicalize(&p)?;
            ensure!(
                !canonical.starts_with(catalog_root),
                "catalog-managed files cannot be originals or sidecar sources"
            );
            ensure!(
                quick_evidence_matches(&canonical, &evidence)?,
                "candidate alias changed during resolution"
            );
            Ok((NativePath::from_path(&canonical), evidence))
        });
        match checked {
            Ok((canonical, evidence)) if expected == Some(evidence.hash.as_str()) => {
                candidates.push(Candidate {
                    path: path.clone(),
                    status: MatchStatus::Matched,
                    detail: "Full content digest matches".into(),
                });
                matches.push((canonical, evidence));
            }
            Ok(_) => candidates.push(Candidate {
                path: path.clone(),
                status: MatchStatus::Mismatch,
                detail: if expected.is_none() {
                    "No retained full-file fingerprint"
                } else {
                    "Full content digest differs"
                }
                .into(),
            }),
            Err(error) => {
                let missing = error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound);
                candidates.push(Candidate {
                    path: path.clone(),
                    status: if missing {
                        MatchStatus::Missing
                    } else {
                        MatchStatus::Unavailable
                    },
                    detail: error.to_string(),
                });
            }
        }
    }
    if matches.len() == 1 {
        (
            MatchStatus::Matched,
            "Unique verified candidate".into(),
            candidates,
            matches.pop(),
        )
    } else if matches.len() > 1 {
        (
            MatchStatus::Ambiguous,
            "Multiple candidates have matching content; select explicitly".into(),
            candidates,
            None,
        )
    } else {
        let status = if candidates
            .iter()
            .any(|c| c.status == MatchStatus::Unavailable)
        {
            MatchStatus::Unavailable
        } else if candidates.iter().any(|c| c.status == MatchStatus::Mismatch) {
            MatchStatus::Mismatch
        } else {
            MatchStatus::Missing
        };
        (status, "No verified candidate".into(), candidates, None)
    }
}
fn prepare_sources(
    db: &Connection,
    plan: &str,
    sequence: i64,
    asset: &str,
    request: &RelinkScope,
    item: &ItemData,
    catalog_root: &Path,
) -> Result<()> {
    let mut stmt=db.prepare("SELECT s.id,s.kind,s.locator,s.display,s.availability,s.current_observation,o.provenance FROM metadata_sources s LEFT JOIN metadata_observations o ON o.id=s.current_observation WHERE s.asset_id=?1 AND s.kind IN ('embedded','sidecar') ORDER BY s.id")?;
    for row in stmt.query_map([asset], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Vec<u8>>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, Option<i64>>(5)?,
            r.get::<_, Option<String>>(6)?,
        ))
    })? {
        let (id, kind, locator, display, availability, observation, provenance) = row?;
        let override_paths = exception(db, plan, "source", &id.to_string())?;
        let explicit_exclusion = override_paths.as_ref().is_some_and(Vec::is_empty);
        let paths = if let Some(paths) = override_paths {
            paths
        } else if kind == "embedded" {
            if locator == item.old_location {
                item.destination.iter().cloned().collect()
            } else {
                vec![]
            }
        } else {
            sidecar_paths(request, item, source_native(db, id, item, &locator)?)?
        };
        let expected = provenance
            .map(|v| serde_json::from_str::<serde_json::Value>(&v))
            .transpose()?
            .and_then(|v| {
                v.get("file_revision")
                    .and_then(|v| v.get("blake3"))
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
            });
        let expected = if kind == "embedded" && observation.is_none() {
            item.fingerprint.clone()
        } else {
            expected
        };
        let (status, detail, candidates, selected) = if explicit_exclusion {
            (
                MatchStatus::Excluded,
                "Explicitly excluded".into(),
                vec![],
                None,
            )
        } else if kind == "embedded" && locator == item.old_location {
            match (item.destination.as_ref(), item.evidence.as_ref()) {
                (Some(path), Some(evidence))
                    if expected.as_deref() == Some(evidence.hash.as_str()) =>
                {
                    (
                        MatchStatus::Matched,
                        "Embedded source follows its verified original".into(),
                        vec![Candidate {
                            path: path.clone(),
                            status: MatchStatus::Matched,
                            detail: "Same file evidence as original".into(),
                        }],
                        Some((path.clone(), evidence.clone())),
                    )
                }
                _ => (
                    MatchStatus::Mismatch,
                    "Embedded observation does not match the planned original".into(),
                    vec![],
                    None,
                ),
            }
        } else {
            evaluate(&paths, expected.as_deref(), catalog_root)
        };
        let (destination, evidence) = selected
            .map(|(p, e)| (Some(p), Some(e)))
            .unwrap_or((None, None));
        let data = SourceData {
            embedded: kind == "embedded",
            old_native: source_native(db, id, item, &locator)?,
            old_tag: get_source_tag(db, id)?,
            old_locator: locator,
            old_display: display,
            old_availability: availability,
            observation,
            candidates,
            destination,
            evidence,
        };
        db.execute(
            "INSERT INTO storage_source_items VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                plan,
                sequence,
                id,
                status.name(),
                detail,
                data.destination
                    .as_ref()
                    .map(|p| p.to_path().map(|p| location_bytes(&p)))
                    .transpose()?,
                json(&data)?
            ],
        )?;
    }
    Ok(())
}
fn sidecar_paths(
    request: &RelinkScope,
    item: &ItemData,
    native: Option<NativePath>,
) -> Result<Vec<NativePath>> {
    let Some(native) = native else {
        return Ok(vec![]);
    };
    if let RelinkScope::Prefix { from, destinations } = request {
        let mut result = Vec::new();
        for destination in destinations {
            if let Some(path) = remap(&PathReference::Native(native.clone()), from, destination)? {
                result.push(path);
            }
        }
        if !result.is_empty() {
            return Ok(result);
        }
    }
    let Some(destination) = &item.destination else {
        return Ok(vec![]);
    };
    let Some(binding) = item.old_binding.as_ref() else {
        return Ok(vec![]);
    };
    let old = components(&PathReference::Native(binding.native_path.clone()))?;
    let sidecar = components(&PathReference::Native(native))?;
    if old.windows != sidecar.windows
        || old.root != sidecar.root
        || old.names.is_empty()
        || sidecar.names.is_empty()
        || old.names[..old.names.len() - 1] != sidecar.names[..sidecar.names.len() - 1]
    {
        return Ok(vec![]);
    }
    let old_name = old.names.last().context("old file name missing")?;
    let side_name = sidecar.names.last().context("sidecar name missing")?;
    let split = |name: &[u16]| name.iter().rposition(|v| *v == 46).filter(|i| *i > 0);
    let old_stem = &old_name[..split(old_name).unwrap_or(old_name.len())];
    let Some(dot) = split(side_name) else {
        return Ok(vec![]);
    };
    let side_stem = &side_name[..dot];
    let extension = native_component(sidecar.windows, &side_name[dot + 1..])?;
    let new = destination.to_path()?;
    let Some(parent) = new.parent() else {
        return Ok(vec![]);
    };
    let mut result = Vec::new();
    let base = if side_stem == old_stem {
        new.file_stem().map(std::ffi::OsStr::to_os_string)
    } else if side_stem == old_name {
        new.file_name().map(std::ffi::OsStr::to_os_string)
    } else {
        None
    };
    if let Some(mut name) = base {
        name.push(".");
        name.push(extension);
        result.push(NativePath::from_path(&parent.join(name)));
    } else {
        result.push(NativePath::from_path(
            &parent.join(native_component(sidecar.windows, side_name)?),
        ));
    }

    Ok(result)
}
const SOURCE_COLLISION: &str = "EXISTS(SELECT 1 FROM storage_source_items j JOIN metadata_sources js ON js.id=j.source_id JOIN metadata_sources ss ON ss.id=s.source_id WHERE j.plan=s.plan AND j.sequence=s.sequence AND j.source_id!=s.source_id AND j.status='matched' AND j.destination=s.destination AND js.kind=ss.kind) OR EXISTS(SELECT 1 FROM metadata_sources old JOIN metadata_sources own ON own.id=s.source_id WHERE old.asset_id=own.asset_id AND old.kind=own.kind AND old.locator=s.destination AND old.id!=own.id AND NOT EXISTS(SELECT 1 FROM storage_source_items j WHERE j.plan=s.plan AND j.source_id=old.id AND j.status='matched'))";
fn mark_collisions(db: &Connection, plan: &str) -> Result<()> {
    db.execute("UPDATE storage_items SET status='ambiguous',detail='Destination path or file object is shared by multiple selected assets' WHERE plan=?1 AND status='matched' AND (destination IN (SELECT destination FROM storage_items WHERE plan=?1 AND status='matched' GROUP BY destination HAVING COUNT(*)>1) OR file_key IN (SELECT file_key FROM storage_items WHERE plan=?1 AND status='matched' GROUP BY file_key HAVING COUNT(*)>1))",[plan])?;
    db.execute("UPDATE storage_items SET status='ambiguous',detail='Destination already belongs to another unmoved asset' WHERE plan=?1 AND status='matched' AND EXISTS(SELECT 1 FROM assets a WHERE a.location=storage_items.destination AND a.id!=storage_items.asset_id AND NOT EXISTS(SELECT 1 FROM storage_items j WHERE j.plan=?1 AND j.asset_id=a.id AND j.status='matched'))",[plan])?;
    db.execute("UPDATE storage_items SET status='ambiguous',detail='Destination object belongs to another unmoved asset' WHERE plan=?1 AND status='matched' AND EXISTS(SELECT 1 FROM storage_bindings b WHERE b.file_key=storage_items.file_key AND b.asset_id!=storage_items.asset_id AND NOT EXISTS(SELECT 1 FROM storage_items j WHERE j.plan=?1 AND j.asset_id=b.asset_id AND j.status='matched'))",[plan])?;
    db.execute(&format!("UPDATE storage_source_items AS s SET status='ambiguous',detail='Metadata locator collides with another source' WHERE s.plan=?1 AND s.status='matched' AND ({SOURCE_COLLISION})"),[plan])?;
    Ok(())
}
fn validate_collisions(db: &Connection, plan: &str) -> Result<()> {
    let count:i64=db.query_row("SELECT COUNT(*) FROM storage_items i WHERE i.plan=?1 AND i.status='matched' AND (EXISTS(SELECT 1 FROM storage_items j WHERE j.plan=i.plan AND j.status='matched' AND j.asset_id!=i.asset_id AND (j.destination=i.destination OR j.file_key=i.file_key)) OR EXISTS(SELECT 1 FROM assets a WHERE a.location=i.destination AND a.id!=i.asset_id AND NOT EXISTS(SELECT 1 FROM storage_items j WHERE j.plan=i.plan AND j.asset_id=a.id AND j.status='matched')) OR EXISTS(SELECT 1 FROM storage_bindings b WHERE b.file_key=i.file_key AND b.asset_id!=i.asset_id AND NOT EXISTS(SELECT 1 FROM storage_items j WHERE j.plan=i.plan AND j.asset_id=b.asset_id AND j.status='matched')))",[plan],|r|r.get(0))?;
    ensure!(
        count == 0,
        "selected destination collision; revise the plan"
    );
    let sources: i64 = db.query_row(
        &format!("SELECT COUNT(*) FROM storage_source_items s JOIN storage_items i ON i.plan=s.plan AND i.sequence=s.sequence WHERE s.plan=?1 AND s.status='matched' AND i.status='matched' AND ({SOURCE_COLLISION})"),
        [plan], |r| r.get(0),
    )?;
    ensure!(
        sources == 0,
        "selected metadata source collision; revise the plan"
    );
    Ok(())
}
fn quick_evidence_matches(path: &Path, expected: &Evidence) -> Result<bool> {
    let file = open_regular(path)?;
    let metadata = file.metadata()?;
    Ok(object_key(&file)? == expected.object
        && metadata.len() == expected.length
        && metadata
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
            == expected.modified_ns)
}
fn verify_destination(path: Option<&NativePath>, expected: Option<&Evidence>) -> Result<()> {
    let path = path.context("missing planned destination")?.to_path()?;
    ensure!(
        Some(&read_evidence(&path)?) == expected,
        "destination content or file identity changed after preview: {}",
        path.display()
    );
    Ok(())
}
fn open_regular(path: &Path) -> Result<File> {
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_file(),
        "candidate is not an ordinary file (links refused)"
    );
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000);
    }
    let file = options.open(path)?;
    ensure!(
        file.metadata()?.file_type().is_file(),
        "opened candidate is not an ordinary file"
    );
    Ok(file)
}
fn read_evidence(path: &Path) -> Result<Evidence> {
    let mut file = open_regular(path)?;
    let before = file.metadata()?;
    let object = object_key(&file)?;
    let modified_ns = before
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let mut hasher = blake3::Hasher::new();
    let mut count = 0u64;
    let mut buf = [0; 65536];
    let mut bounded = (&mut file).take(before.len().saturating_add(1));
    loop {
        let n = bounded.read(&mut buf)?;
        if n == 0 {
            break;
        }
        count += n as u64;
        hasher.update(&buf[..n]);
    }
    let after = file.metadata()?;
    let reopened = open_regular(path)?;
    ensure!(
        count == before.len()
            && before.len() == after.len()
            && before.modified()? == after.modified()?
            && object == object_key(&reopened)?
            && reopened.metadata()?.modified()? == before.modified()?,
        "candidate changed while hashing"
    );
    Ok(Evidence {
        hash: hasher.finalize().to_hex().to_string(),
        length: count,
        modified_ns,
        object,
    })
}
#[cfg(unix)]
fn object_key(file: &File) -> Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let m = file.metadata()?;
    Ok((m.dev(), m.ino()))
}
#[cfg(windows)]
fn object_key(file: &File) -> Result<(u64, u64)> {
    use std::os::windows::io::AsRawHandle;
    #[repr(C)]
    struct Info {
        attributes: u32,
        creation: [u32; 2],
        access: [u32; 2],
        write: [u32; 2],
        volume: u32,
        size_high: u32,
        size_low: u32,
        links: u32,
        index_high: u32,
        index_low: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandle(handle: *mut std::ffi::c_void, info: *mut Info) -> i32;
    }
    let mut info = std::mem::MaybeUninit::<Info>::uninit();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let info = unsafe { info.assume_init() };
    ensure!(
        info.attributes & 0x400 == 0,
        "reparse-point candidates refused"
    );
    Ok((
        info.volume as u64,
        ((info.index_high as u64) << 32) | info.index_low as u64,
    ))
}

#[cfg(test)]
mod folder_alias_tests {
    use super::*;

    #[test]
    fn windows_folder_aliases_preserve_roots_case_and_exact_utf16() -> Result<()> {
        // These expected folder keys are literal code-unit sequences, independent
        // of the component parser. No Windows filesystem is needed for this test.
        for (root, verbatim_root) in [
            (r"Q:\", r"\\?\Q:\"),
            (r"\\SeRvEr\ShArE\", r"\\?\UNC\SeRvEr\ShArE\"),
        ] {
            let root_units: Vec<u16> = root.encode_utf16().collect();
            let first: Vec<u16> = format!("{root}MiXeD").encode_utf16().collect();
            let mut leaf: Vec<u16> = format!("{root}MiXeD\\東京🚀").encode_utf16().collect();
            leaf.push(0xd800); // An unpaired surrogate is identity, not display text.
            let expected = [root_units, first, leaf.clone()]
                .map(NativePath::WindowsWide)
                .to_vec();
            let mut regular = leaf.clone();
            regular.extend("\\photo.jpg".encode_utf16());
            let mut extended: Vec<u16> = format!("{verbatim_root}MiXeD\\東京🚀")
                .encode_utf16()
                .collect();
            extended.push(0xd800);
            extended.extend("\\photo.jpg".encode_utf16());
            for input in [regular, extended] {
                let tagged = NativePath::WindowsWide(input);
                let before = tagged.clone();
                let actual = native_folder_chain(&tagged)?;
                assert_eq!(
                    actual.into_iter().map(|(path, _)| path).collect::<Vec<_>>(),
                    expected
                );
                assert_eq!(tagged, before);
            }
            // Lossy display would collapse these two names; folder identities must not.
            *leaf.last_mut().unwrap() = 0xd801;
            leaf.extend("\\photo.jpg".encode_utf16());
            let different = native_folder_chain(&NativePath::WindowsWide(leaf))?;
            assert_ne!(&different.last().unwrap().0, expected.last().unwrap());
            let different_case =
                NativePath::WindowsWide(format!("{root}mixed\\photo.jpg").encode_utf16().collect());
            assert_ne!(native_folder_chain(&different_case)?[1].0, expected[1]);
        }
        Ok(())
    }
}

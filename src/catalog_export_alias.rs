//! Conservative export destination protection over a bounded derived path index.
//! Projection is filesystem-free; existing-file admission additionally resolves
//! current parent directories, so a replaced inode or redirected parent is not
//! vouched for by historical storage_bindings.file_key evidence.
use crate::storage_volume::{self, NativePath};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

pub(crate) const SCHEMA: &str = "
CREATE TABLE export_alias_state(id INTEGER PRIMARY KEY CHECK(id=1),unbound INTEGER NOT NULL CHECK(unbound>=0));
INSERT INTO export_alias_state SELECT 1,COUNT(*) FROM assets a LEFT JOIN storage_bindings b ON b.asset_id=a.id WHERE b.asset_id IS NULL;
CREATE TRIGGER export_alias_asset_added AFTER INSERT ON assets BEGIN UPDATE export_alias_state SET unbound=unbound+1 WHERE id=1; END;
CREATE TRIGGER export_alias_asset_removed BEFORE DELETE ON assets WHEN NOT EXISTS(SELECT 1 FROM storage_bindings WHERE asset_id=old.id) BEGIN UPDATE export_alias_state SET unbound=unbound-1 WHERE id=1; END;
CREATE TRIGGER export_alias_binding_removed AFTER DELETE ON storage_bindings WHEN EXISTS(SELECT 1 FROM assets WHERE id=old.asset_id) BEGIN UPDATE export_alias_state SET unbound=unbound+1 WHERE id=1; END;
CREATE TABLE export_alias_directories(id INTEGER PRIMARY KEY,native_path TEXT NOT NULL UNIQUE,members INTEGER NOT NULL DEFAULT 0 CHECK(members>=0));
CREATE INDEX export_alias_active_directories ON export_alias_directories(id) WHERE members>0;
CREATE TABLE export_alias_paths(asset_id TEXT PRIMARY KEY REFERENCES storage_bindings(asset_id) ON DELETE CASCADE,parent INTEGER NOT NULL REFERENCES export_alias_directories(id),ascii_path TEXT,prefix TEXT NOT NULL,filename TEXT);
CREATE INDEX export_alias_ascii ON export_alias_paths(ascii_path,asset_id) WHERE ascii_path IS NOT NULL;
CREATE INDEX export_alias_prefix ON export_alias_paths(prefix,asset_id);
CREATE INDEX export_alias_unicode_prefix ON export_alias_paths(prefix,asset_id) WHERE ascii_path IS NULL;
CREATE INDEX export_alias_filename ON export_alias_paths(parent,filename,asset_id);
CREATE TABLE export_alias_dirty(asset_id TEXT PRIMARY KEY REFERENCES storage_bindings(asset_id) ON DELETE CASCADE);
INSERT INTO export_alias_dirty SELECT asset_id FROM storage_bindings;
CREATE TRIGGER export_alias_binding_added AFTER INSERT ON storage_bindings BEGIN INSERT OR IGNORE INTO export_alias_dirty VALUES(new.asset_id); UPDATE export_alias_state SET unbound=unbound-1 WHERE id=1; END;
CREATE TRIGGER export_alias_binding_changed AFTER UPDATE OF native_path ON storage_bindings WHEN old.native_path!=new.native_path BEGIN INSERT OR IGNORE INTO export_alias_dirty VALUES(new.asset_id); END;
CREATE TRIGGER export_alias_path_added AFTER INSERT ON export_alias_paths BEGIN UPDATE export_alias_directories SET members=members+1 WHERE id=new.parent; END;
CREATE TRIGGER export_alias_path_deleted AFTER DELETE ON export_alias_paths BEGIN UPDATE export_alias_directories SET members=members-1 WHERE id=old.parent; END;
CREATE TRIGGER export_alias_path_parent AFTER UPDATE OF parent ON export_alias_paths WHEN old.parent!=new.parent BEGIN UPDATE export_alias_directories SET members=members-1 WHERE id=old.parent; UPDATE export_alias_directories SET members=members+1 WHERE id=new.parent; END;
";
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct AliasLimits {
    /// Current distinct parent-directory resolutions per overwrite, not images.
    pub directories: usize,
    /// Current candidate file stats across every indexed query in one admission.
    pub candidates: usize,
}
impl Default for AliasLimits {
    fn default() -> Self {
        Self {
            directories: 4096,
            candidates: 256,
        }
    }
}
#[derive(Debug, Serialize)]
pub struct AliasProgress {
    pub projected: usize,
    pub pending: bool,
    pub unbound: u64,
}
#[derive(Debug, Serialize)]
pub struct AliasProof {
    pub overwrite: bool,
    pub directories: usize,
    pub candidates: usize,
}
struct Projection {
    parent: NativePath,
    ascii: Option<String>,
    prefix: String,
    filename: Option<String>,
}
fn projection(path: &NativePath) -> Result<Projection> {
    let (windows, raw): (bool, Vec<u16>) = match path {
        NativePath::UnixBytes(bytes) => (false, bytes.iter().map(|b| u16::from(*b)).collect()),
        NativePath::WindowsWide(units) => (true, units.clone()),
    };
    ensure!(
        !raw.is_empty() && raw.len() <= 32768 && !raw.contains(&0),
        "invalid export alias native path"
    );
    let separator = |v: u16| v == 47 || (windows && v == 92);
    let last = raw
        .iter()
        .rposition(|u| separator(*u))
        .context("original has no parent directory")?;
    ensure!(last + 1 < raw.len(), "original has no filename");
    let parent_units = if last == 0 {
        &raw[..1]
    } else if windows && raw.get(last.saturating_sub(1)) == Some(&58) {
        &raw[..last + 1]
    } else {
        &raw[..last]
    };
    let parent = if windows {
        NativePath::WindowsWide(parent_units.to_vec())
    } else {
        NativePath::UnixBytes(parent_units.iter().map(|v| *v as u8).collect())
    };
    let mut units = raw.clone();
    if windows {
        for u in &mut units {
            if *u == 92 {
                *u = 47;
            }
        }
        if units.starts_with(&[47, 47, 63, 47]) {
            if units.get(4..8).is_some_and(|p| {
                p.iter().all(|v| *v < 128)
                    && p.iter()
                        .map(|v| (*v as u8).to_ascii_lowercase())
                        .eq(b"unc/".iter().copied())
            }) {
                units = [vec![47, 47], units[8..].to_vec()].concat();
            } else {
                ensure!(
                    units.get(5) == Some(&58),
                    "unsupported Windows device path requires mapping"
                );
                units = units[4..].to_vec();
            }
        }
        ensure!(
            !units.starts_with(&[47, 47, 46, 47]),
            "Windows device path requires mapping"
        );
        ensure!(
            units.starts_with(&[47, 47])
                || (units.len() > 3
                    && units[1] == 58
                    && units[2] == 47
                    && units[0] < 128
                    && (units[0] as u8).is_ascii_alphabetic()),
            "relative Windows original requires mapping"
        );
    } else {
        ensure!(
            units.first() == Some(&47),
            "relative Unix original requires mapping"
        );
    }
    ensure!(
        !units
            .split(|u| *u == 47)
            .any(|part| part == [46] || part == [46, 46]),
        "dot path original requires mapping"
    );
    let tag = if windows { "w:" } else { "u:" };
    let ascii_text = |v: &[u16]| -> String {
        v.iter()
            .map(|u| (*u as u8).to_ascii_lowercase() as char)
            .collect()
    };
    // Win32 accepts short-name and trailing-dot/space aliases. Coarsen these
    // spellings rather than trusting historical file IDs or ASCII-only equality.
    if windows {
        let mut normalized = Vec::with_capacity(units.len());
        for (index, part) in units.split(|u| *u == 47).enumerate() {
            if index != 0 {
                normalized.push(47);
            }
            if part.is_empty() {
                continue;
            }
            let end = part
                .iter()
                .rposition(|u| ![32, 46].contains(u))
                .map(|i| i + 1)
                .unwrap_or(0);
            ensure!(
                end > 0,
                "ambiguous Windows dot/space component requires mapping"
            );
            let part = &part[..end];
            ensure!(
                part != [46] && part != [46, 46],
                "Windows dot path requires mapping"
            );
            ensure!(
                !part.contains(&58) || (index == 0 && part.len() == 2 && part[1] == 58),
                "Windows stream/device path is not a photo destination"
            );
            normalized.extend_from_slice(part);
        }
        units = normalized;
    }
    let first_non_ascii = units
        .iter()
        .position(|u| *u > 127 || (windows && *u == 126));
    let ascii = first_non_ascii
        .is_none()
        .then(|| format!("{tag}{}", ascii_text(&units)));
    let prefix_end = units[..first_non_ascii.unwrap_or(units.len())]
        .iter()
        .rposition(|u| *u == 47)
        .context("original root alias cannot be projected")?
        + 1;
    let prefix = format!("{tag}{}", ascii_text(&units[..prefix_end]));
    let name_start = units.iter().rposition(|u| *u == 47).unwrap() + 1;
    let name = &units[name_start..];
    let filename = name
        .iter()
        .all(|u| *u < 128 && !(windows && *u == 126))
        .then(|| ascii_text(name));
    Ok(Projection {
        parent,
        ascii,
        prefix,
        filename,
    })
}
fn pending(db: &Connection) -> Result<bool> {
    Ok(
        db.query_row("SELECT EXISTS(SELECT 1 FROM export_alias_dirty)", [], |r| {
            r.get(0)
        })?,
    )
}
/// Call in an admitted catalog transaction. Each invocation projects at most512
/// paths and2MiB of serialized input; never stats or opens original files.
/// Triggered dirtiness ensures skipped/failed work cannot authorize an export.
pub fn reconcile_paths(db: &Connection, limit: usize) -> Result<AliasProgress> {
    ensure!(
        !db.is_autocommit(),
        "alias reconciliation requires a catalog transaction"
    );
    ensure!((1..=512).contains(&limit), "alias projection batch bound");
    let mut statement=db.prepare("SELECT b.asset_id,b.native_path,length(CAST(b.native_path AS BLOB)) FROM export_alias_dirty d CROSS JOIN storage_bindings b ON b.asset_id=d.asset_id ORDER BY d.asset_id LIMIT ?1")?;
    let mut rows = statement.query([limit as i64])?;
    let mut batch = Vec::new();
    let mut bytes = 0usize;
    while let Some(row) = rows.next()? {
        let length: i64 = row.get(2)?;
        ensure!(
            (0..=256 * 1024).contains(&length),
            "alias projection input bound"
        );
        let asset: String = row.get(0)?;
        let encoded: String = row.get(1)?;
        ensure!(
            encoded.len() <= 256 * 1024 && asset.len() <= 256,
            "alias projection input bound"
        );
        bytes += asset.len() + encoded.len();
        if bytes > 2 * 1024 * 1024 {
            break;
        }
        batch.push((asset, encoded));
    }
    drop(rows);
    drop(statement);
    for (asset, encoded) in &batch {
        let native: NativePath = serde_json::from_str(encoded)?;
        let p = projection(&native)?;
        let parent = serde_json::to_string(&p.parent)?;
        db.execute(
            "INSERT OR IGNORE INTO export_alias_directories(native_path) VALUES(?1)",
            [&parent],
        )?;
        let id: i64 = db.query_row(
            "SELECT id FROM export_alias_directories WHERE native_path=?1",
            [parent],
            |r| r.get(0),
        )?;
        db.execute("INSERT INTO export_alias_paths VALUES(?1,?2,?3,?4,?5) ON CONFLICT(asset_id) DO UPDATE SET parent=excluded.parent,ascii_path=excluded.ascii_path,prefix=excluded.prefix,filename=excluded.filename",params![asset,id,p.ascii,p.prefix,p.filename])?;
        db.execute("DELETE FROM export_alias_dirty WHERE asset_id=?1", [asset])?;
    }
    Ok(AliasProgress {
        projected: batch.len(),
        pending: pending(db)?,
        unbound: db.query_row(
            "SELECT unbound FROM export_alias_state WHERE id=1",
            [],
            |r| r.get(0),
        )?,
    })
}
struct Admission<'a> {
    db: &'a Connection,
    destination: &'a Path,
    target: Option<(u64, u128)>,
    limits: AliasLimits,
    proof: AliasProof,
}
impl Admission<'_> {
    fn candidate(&mut self, encoded: String) -> Result<()> {
        self.proof.candidates += 1;
        ensure!(
            self.proof.candidates <= self.limits.candidates,
            "export alias ambiguity exceeds candidate budget; reconcile or choose an unambiguous destination"
        );
        let native: NativePath = serde_json::from_str(&encoded)?;
        let source = native
            .to_path()
            .context("foreign original path needs explicit mapping before overwrite")?;
        match fs::metadata(&source) {
            Ok(metadata) => {
                ensure!(
                    metadata.is_file(),
                    "catalog original changed file type; reconcile before overwrite"
                );
                if let Some(target) = self.target {
                    ensure!(
                        storage_volume::object_key(&source, &metadata)? != target,
                        "export destination aliases a catalog original"
                    );
                } else {
                    // An existing source cannot alias a currently absent name;
                    // the publisher still enforces the exact absence snapshot.
                    ensure!(
                        !self.destination.try_exists()?,
                        "export destination appeared during alias validation"
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && self.target.is_some() => {
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => bail!(
                "missing catalog original has an ambiguous export spelling; map it or choose another destination"
            ),
            Err(error) => return Err(error).context("cannot resolve catalog original alias"),
        }
        Ok(())
    }
    fn candidates(&mut self, sql: &str, arguments: impl rusqlite::Params) -> Result<()> {
        let db = self.db;
        let mut statement = db.prepare(sql)?;
        let mut rows = statement.query(arguments)?;
        while let Some(row) = rows.next()? {
            self.candidate(row.get(0)?)?;
        }
        Ok(())
    }
    fn remaining(&self) -> i64 {
        self.limits.candidates.saturating_sub(self.proof.candidates) as i64 + 1
    }
}
/// Must run within the caller's authoritative catalog snapshot/transaction, and
/// again at publication. This is bounded current-path evidence, not a promise to
/// freeze arbitrary external filesystem mutations after returning.
pub fn protect_destination(
    db: &Connection,
    destination: &Path,
    limits: AliasLimits,
) -> Result<AliasProof> {
    ensure!(
        !db.is_autocommit(),
        "alias validation requires an authoritative catalog transaction"
    );
    ensure!(
        limits.directories <= 65536 && (1..=4096).contains(&limits.candidates),
        "export alias validation budget"
    );
    let unbound: i64 = db.query_row(
        "SELECT unbound FROM export_alias_state WHERE id=1",
        [],
        |r| r.get(0),
    )?;
    ensure!(
        unbound == 0,
        "catalog originals have undeclared native paths; bind or explicitly map them before export"
    );
    ensure!(
        !pending(db)?,
        "export alias index has pending path projections; run bounded reconciliation first"
    );
    ensure!(
        destination.is_absolute(),
        "absolute export destination required"
    );
    let native = NativePath::from_path(destination);
    let p = projection(&native)?;
    let metadata = match fs::symlink_metadata(destination) {
        Ok(m) => Some(m),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let target = metadata
        .as_ref()
        .map(|m| {
            ensure!(
                m.is_file() && !m.file_type().is_symlink(),
                "export destination is not an ordinary file"
            );
            Ok(storage_volume::object_key(destination, m)?)
        })
        .transpose()?;
    let mut admission = Admission {
        db,
        destination,
        target,
        limits,
        proof: AliasProof {
            overwrite: target.is_some(),
            directories: 0,
            candidates: 0,
        },
    };
    if let Some(exact) = &p.ascii {
        let known: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM export_alias_paths WHERE ascii_path=?1)",
            [exact],
            |r| r.get(0),
        )?;
        ensure!(
            !known,
            "export destination spelling may identify a catalog original (case-insensitive conservative guard)"
        );
    }
    // Unicode comparison is deliberately not guessed. Candidates include every
    // ancestor ASCII prefix before any non-ASCII component, in either spelling.
    let comparison = p.ascii.as_ref().unwrap_or(&p.prefix);
    for (index, character) in comparison.char_indices() {
        if character == '/' {
            let prefix = &comparison[..index + 1];
            admission.candidates("SELECT b.native_path FROM export_alias_paths p INDEXED BY export_alias_unicode_prefix CROSS JOIN storage_bindings b ON b.asset_id=p.asset_id WHERE p.ascii_path IS NULL AND p.prefix=?1 ORDER BY p.asset_id LIMIT ?2",params![prefix,admission.remaining()])?;
        }
    }
    if p.ascii.is_none() {
        admission.candidates("SELECT b.native_path FROM export_alias_paths p CROSS JOIN storage_bindings b ON b.asset_id=p.asset_id WHERE p.prefix>=?1 AND p.prefix<?2 ORDER BY p.prefix,p.asset_id LIMIT ?3",params![p.prefix,format!("{}\u{10ffff}",p.prefix),admission.remaining()])?;
    }
    if let Some(target) = target {
        let parent = destination.parent().context("destination parent missing")?;
        let parent_metadata = fs::metadata(parent)?;
        let parent_key = storage_volume::object_key(parent, &parent_metadata)?;
        let mut statement=db.prepare("SELECT id,native_path FROM export_alias_directories INDEXED BY export_alias_active_directories WHERE members>0 ORDER BY id LIMIT ?1")?;
        let mut rows = statement.query([limits.directories as i64 + 1])?;
        while let Some(row) = rows.next()? {
            admission.proof.directories += 1;
            ensure!(
                admission.proof.directories <= limits.directories,
                "export overwrite requires more directory reconciliation than configured; increase the explicit directory budget or select a new destination"
            );
            let id: i64 = row.get(0)?;
            let encoded: String = row.get(1)?;
            let native: NativePath = serde_json::from_str(&encoded)?;
            let directory = native
                .to_path()
                .context("foreign original directory requires explicit mapping before overwrite")?;
            let m = match fs::metadata(&directory) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e).context("cannot validate original directory"),
            };
            ensure!(
                m.is_dir(),
                "original directory changed type; reconcile before export"
            );
            if storage_volume::object_key(&directory, &m)? != parent_key {
                continue;
            }
            if let Some(filename) = &p.filename {
                admission.candidates("SELECT b.native_path FROM export_alias_paths p CROSS JOIN storage_bindings b ON b.asset_id=p.asset_id WHERE p.parent=?1 AND p.filename=?2 ORDER BY p.asset_id LIMIT ?3",params![id,filename,admission.remaining()])?;
                admission.candidates("SELECT b.native_path FROM export_alias_paths p CROSS JOIN storage_bindings b ON b.asset_id=p.asset_id WHERE p.parent=?1 AND p.filename IS NULL ORDER BY p.asset_id LIMIT ?2",params![id,admission.remaining()])?;
            } else {
                admission.candidates("SELECT b.native_path FROM export_alias_paths p CROSS JOIN storage_bindings b ON b.asset_id=p.asset_id WHERE p.parent=?1 ORDER BY p.filename,p.asset_id LIMIT ?2",params![id,admission.remaining()])?;
            }
            ensure!(
                storage_volume::object_key(&directory, &fs::metadata(&directory)?)? == parent_key,
                "original parent changed during export validation"
            );
        }
        let current_destination = fs::symlink_metadata(destination)?;
        ensure!(
            current_destination.is_file()
                && !current_destination.file_type().is_symlink()
                && storage_volume::object_key(parent, &fs::metadata(parent)?)? == parent_key
                && storage_volume::object_key(destination, &current_destination)? == target,
            "destination changed during export validation"
        );
    } else {
        ensure!(
            !destination.try_exists()?,
            "destination appeared during alias validation"
        );
    }
    Ok(admission.proof)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn database() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("PRAGMA foreign_keys=ON; CREATE TABLE assets(id TEXT PRIMARY KEY); CREATE TABLE storage_bindings(asset_id TEXT PRIMARY KEY REFERENCES assets(id) ON DELETE CASCADE,native_path TEXT NOT NULL,file_key TEXT);").unwrap();
        db.execute_batch(SCHEMA).unwrap();
        db
    }
    fn bind(db: &Connection, id: &str, path: &Path, key: Option<&str>) {
        db.execute("INSERT INTO assets VALUES(?1)", [id]).unwrap();
        db.execute(
            "INSERT INTO storage_bindings VALUES(?1,?2,?3)",
            params![
                id,
                serde_json::to_string(&NativePath::from_path(path)).unwrap(),
                key
            ],
        )
        .unwrap();
    }
    fn reconcile(db: &Connection) {
        loop {
            let tx = db.unchecked_transaction().unwrap();
            let p = reconcile_paths(&tx, 512).unwrap();
            tx.commit().unwrap();
            if !p.pending {
                break;
            }
        }
    }
    fn guard(db: &Connection, path: &Path, limits: AliasLimits) -> Result<AliasProof> {
        let tx = db.unchecked_transaction()?;
        protect_destination(&tx, path, limits)
    }
    #[test]
    fn replaced_original_and_null_historical_key_cannot_be_overwritten_through_case_alias() {
        for historical in [None, Some("0:obsolete")] {
            let root = tempfile::tempdir().unwrap();
            let folder = fs::canonicalize(root.path()).unwrap();
            let original = folder.join("Original.jpg");
            fs::write(&original, b"old").unwrap();
            let db = database();
            bind(&db, "original", &original, historical);
            reconcile(&db);
            fs::rename(&original, folder.join("previous")).unwrap();
            fs::write(&original, b"external replacement").unwrap();
            let alias = folder.join("ORIGINAL.JPG");
            assert!(guard(&db, &alias, AliasLimits::default()).is_err());
            assert_eq!(fs::read(&original).unwrap(), b"external replacement");
            let proof = guard(&db, &folder.join("new-export.jpg"), AliasLimits::default()).unwrap();
            assert!(!proof.overwrite);
            assert_eq!(proof.directories, 0);
            assert_eq!(proof.candidates, 0);
        }
    }
    #[test]
    fn unicode_aliases_follow_actual_filesystem_semantics_without_lossy_folding() {
        let root = tempfile::tempdir().unwrap();
        let folder = fs::canonicalize(root.path()).unwrap();
        let db = database();
        for (i, (stored, requested)) in [
            ("É.jpg", "e\u{301}.jpg"),
            ("K.jpg", "k.jpg"),
            ("Σ.jpg", "ς.jpg"),
        ]
        .into_iter()
        .enumerate()
        {
            let original = folder.join(stored);
            fs::write(&original, b"retained").unwrap();
            bind(&db, &i.to_string(), &original, None);
            reconcile(&db);
            let candidate = folder.join(requested);
            let result = guard(&db, &candidate, AliasLimits::default());
            if candidate.exists() {
                assert!(result.is_err(), "actual Unicode alias admitted");
            } else {
                assert!(
                    result.is_ok(),
                    "a distinct absent filesystem name should remain usable: {result:?}"
                );
            }
            assert_eq!(fs::read(original).unwrap(), b"retained");
        }
    }
    #[cfg(unix)]
    #[test]
    fn current_parent_redirect_is_checked_even_when_cached_file_identity_is_stale() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(root.path()).unwrap();
        let first = root.join("first");
        let second = root.join("second");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        fs::write(first.join("photo.jpg"), b"first source").unwrap();
        fs::write(second.join("photo.jpg"), b"new source").unwrap();
        let link = root.join("logical");
        symlink(&first, &link).unwrap();
        let db = database();
        bind(&db, "source", &link.join("photo.jpg"), Some("old:inode"));
        reconcile(&db);
        fs::remove_file(&link).unwrap();
        symlink(&second, &link).unwrap();
        let error = guard(&db, &second.join("photo.jpg"), AliasLimits::default()).unwrap_err();
        assert!(error.to_string().contains("aliases a catalog original"));
        assert_eq!(fs::read(second.join("photo.jpg")).unwrap(), b"new source");
    }
    #[test]
    fn projection_preserves_windows_units_and_covers_verbatim_drive_and_unc_aliases() {
        let wide = |s: &str| NativePath::WindowsWide(s.encode_utf16().collect());
        let plain = projection(&wide("C:\\Photos\\FILE.jpg")).unwrap();
        let verbatim = projection(&wide("\\\\?\\C:\\Photos\\file.JPG")).unwrap();
        assert_eq!(plain.ascii, verbatim.ascii);
        assert_eq!(
            projection(&wide("\\\\Server\\Share\\FILE.jpg"))
                .unwrap()
                .ascii,
            projection(&wide("\\\\?\\UNC\\server\\share\\file.JPG"))
                .unwrap()
                .ascii
        );
        let mut units = "C:\\Photos\\".encode_utf16().collect::<Vec<_>>();
        units.extend([0xd800, 46, 106, 112, 103]);
        let p = projection(&NativePath::WindowsWide(units)).unwrap();
        assert!(p.ascii.is_none());
        assert!(p.filename.is_none());
        assert_eq!(p.prefix, "w:c:/photos/");
        assert_eq!(
            projection(&wide("C:\\Photos\\file.JPG. ")).unwrap().ascii,
            projection(&wide("C:\\Photos\\file.jpg")).unwrap().ascii
        );
        assert!(
            projection(&wide("C:\\Photos\\ORIGIN~1.JPG"))
                .unwrap()
                .filename
                .is_none()
        );
        assert_eq!(
            projection(&wide("\\\\?\\C:\\file.jpg")).unwrap().parent,
            wide("\\\\?\\C:\\")
        );
        assert!(projection(&wide("C:\\Photos\\file.jpg:stream.jpg")).is_err());
        let p = projection(&NativePath::UnixBytes(b"/Photos/\xff.jpg".to_vec())).unwrap();
        assert!(p.ascii.is_none());
        assert_eq!(p.prefix, "u:/photos/");
    }
    #[test]
    fn dirty_paths_and_bounded_parent_or_candidate_exhaustion_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let folder = fs::canonicalize(root.path()).unwrap();
        let db = database();
        let path = folder.join("original.jpg");
        fs::write(&path, b"source").unwrap();
        bind(&db, "source", &path, None);
        assert!(
            guard(&db, &folder.join("new.jpg"), AliasLimits::default())
                .unwrap_err()
                .to_string()
                .contains("pending")
        );
        reconcile(&db);
        let unrelated = folder.join("existing-export.jpg");
        fs::write(&unrelated, b"old export").unwrap();
        assert!(
            guard(
                &db,
                &unrelated,
                AliasLimits {
                    directories: 0,
                    candidates: 256
                }
            )
            .is_err()
        );
        assert!(guard(&db, &unrelated, AliasLimits::default()).is_ok());
        let moved = folder.join("renamed.jpg");
        db.execute(
            "UPDATE storage_bindings SET native_path=?1 WHERE asset_id='source'",
            [serde_json::to_string(&NativePath::from_path(&moved)).unwrap()],
        )
        .unwrap();
        assert!(guard(&db, &unrelated, AliasLimits::default()).is_err());
        reconcile(&db);
        assert!(guard(&db, &moved, AliasLimits::default()).is_err());
        for i in 0..3 {
            let path = folder.join(format!("É{i}.jpg"));
            fs::write(&path, b"source").unwrap();
            bind(&db, &format!("extra{i}"), &path, None);
        }
        reconcile(&db);
        assert!(
            guard(
                &db,
                &folder.join("new.jpg"),
                AliasLimits {
                    directories: 1,
                    candidates: 1
                }
            )
            .unwrap_err()
            .to_string()
            .contains("candidate budget")
        );
    }
    #[test]
    fn projection_failure_rolls_back_and_foreign_parents_require_explicit_mapping() {
        let db = database();
        let root = tempfile::tempdir().unwrap();
        let folder = fs::canonicalize(root.path()).unwrap();
        bind(&db, "a", &folder.join("a.jpg"), None);
        db.execute("INSERT INTO assets VALUES('b')", []).unwrap();
        db.execute(
            "INSERT INTO storage_bindings VALUES('b','malformed',NULL)",
            [],
        )
        .unwrap();
        {
            let tx = db.unchecked_transaction().unwrap();
            assert!(reconcile_paths(&tx, 512).is_err());
        }
        assert_eq!(
            db.query_row("SELECT count(*) FROM export_alias_paths", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert!(pending(&db).unwrap());
        db.execute("DELETE FROM assets WHERE id='b'", []).unwrap();
        #[cfg(unix)]
        let foreign = NativePath::WindowsWide("C:\\Photos\\original.jpg".encode_utf16().collect());
        #[cfg(windows)]
        let foreign = NativePath::UnixBytes(b"/Photos/original.jpg".to_vec());
        db.execute("INSERT INTO assets VALUES('foreign')", [])
            .unwrap();
        db.execute(
            "INSERT INTO storage_bindings VALUES('foreign',?1,NULL)",
            [serde_json::to_string(&foreign).unwrap()],
        )
        .unwrap();
        reconcile(&db);
        let destination = folder.join("export.jpg");
        fs::write(&destination, b"old export").unwrap();
        assert!(
            guard(&db, &destination, AliasLimits::default())
                .unwrap_err()
                .to_string()
                .contains("foreign original directory")
        );
        assert_eq!(fs::read(destination).unwrap(), b"old export");
    }
    #[test]
    fn missing_binding_counter_survives_rebind_and_cascaded_asset_removal() {
        let root = tempfile::tempdir().unwrap();
        let folder = fs::canonicalize(root.path()).unwrap();
        let db = database();
        db.execute("INSERT INTO assets VALUES('unknown')", [])
            .unwrap();
        assert!(
            guard(&db, &folder.join("new.jpg"), AliasLimits::default())
                .unwrap_err()
                .to_string()
                .contains("undeclared")
        );
        let encoded =
            serde_json::to_string(&NativePath::from_path(&folder.join("original.jpg"))).unwrap();
        db.execute(
            "INSERT INTO storage_bindings VALUES('unknown',?1,NULL)",
            [&encoded],
        )
        .unwrap();
        reconcile(&db);
        assert!(guard(&db, &folder.join("new.jpg"), AliasLimits::default()).is_ok());
        db.execute("DELETE FROM storage_bindings WHERE asset_id='unknown'", [])
            .unwrap();
        assert_eq!(
            db.query_row("SELECT unbound FROM export_alias_state", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        db.execute(
            "INSERT INTO storage_bindings VALUES('unknown',?1,NULL)",
            [encoded],
        )
        .unwrap();
        reconcile(&db);
        db.execute("DELETE FROM assets WHERE id='unknown'", [])
            .unwrap();
        assert_eq!(
            db.query_row("SELECT unbound FROM export_alias_state", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM export_alias_directories WHERE members>0",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        assert!(guard(&db, &folder.join("new.jpg"), AliasLimits::default()).is_ok());
    }
    #[test]
    fn ascii_heavy_folders_do_not_expand_unicode_candidate_index_work() {
        for size in [100, 5000] {
            let db = database();
            let tx = db.unchecked_transaction().unwrap();
            for i in 0..size {
                let path = NativePath::UnixBytes(format!("/photos/{i}.jpg").into_bytes());
                tx.execute("INSERT INTO assets VALUES(?1)", [i.to_string()])
                    .unwrap();
                tx.execute(
                    "INSERT INTO storage_bindings VALUES(?1,?2,NULL)",
                    params![i.to_string(), serde_json::to_string(&path).unwrap()],
                )
                .unwrap();
            }
            tx.commit().unwrap();
            reconcile(&db);
            let mut query=db.prepare("SELECT b.native_path FROM export_alias_paths p INDEXED BY export_alias_unicode_prefix CROSS JOIN storage_bindings b ON b.asset_id=p.asset_id WHERE p.ascii_path IS NULL AND p.prefix=?1 ORDER BY p.asset_id LIMIT 257").unwrap();
            assert!(
                query
                    .query(["u:/photos/"])
                    .unwrap()
                    .next()
                    .unwrap()
                    .is_none()
            );
            assert!(query.get_status(rusqlite::StatementStatus::VmStep) < 100);
            assert_eq!(query.get_status(rusqlite::StatementStatus::Sort), 0);
        }
    }
}

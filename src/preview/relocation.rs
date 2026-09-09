//! Incremental relocation keeps the old location authoritative until every
//! immutable object is copied and verified. Cleanup starts only after the switch.
use super::*;
#[derive(Debug, Clone, Serialize)]
pub struct RelocationProgress {
    pub tier: Tier,
    pub phase: String,
    pub processed: usize,
    pub object_bytes: u64,
    pub complete: bool,
}
pub(super) fn lock_root(root: &Path, identity: &str, tier: Tier, layout: Layout) -> Result<File> {
    let marker = root.join(".photocatalog-preview-owner");
    if marker.exists() {
        ensure!(
            fs::symlink_metadata(&marker)?.file_type().is_file(),
            "invalid preview ownership marker"
        );
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(marker)?;
    file.try_lock_exclusive()
        .context("preview location is owned by another process")?;
    let expected = serde_json::to_vec(&(identity, tier, layout))?;
    let length = file.metadata()?.len();
    ensure!(length <= 256, "invalid preview location identity");
    if length == 0 {
        file.write_all(&expected)?;
        file.sync_all()?;
        #[cfg(unix)]
        File::open(root)?.sync_all()?;
    } else {
        let mut actual = vec![0; length as usize];
        file.read_exact(&mut actual)?;
        ensure!(
            actual == expected,
            "preview location belongs to another manifest/tier/layout"
        );
    }
    Ok(file)
}
fn object_path(root: &Path, layout: Layout, key: &str) -> Result<PathBuf> {
    ensure!(
        key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid relocation object key"
    );
    Ok(match layout {
        Layout::Flat => root.join(key),
        Layout::HashPrefix => root.join(&key[..2]).join(&key[2..4]).join(key),
    })
}
fn ensure_destination_parent(root: &Path, layout: Layout, key: &str) -> Result<()> {
    if layout == Layout::Flat {
        return Ok(());
    }
    let mut parent = root.to_path_buf();
    for part in [&key[..2], &key[2..4]] {
        let next = parent.join(part);
        match fs::create_dir(&next) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => ensure!(
                fs::symlink_metadata(&next)?.file_type().is_dir(),
                "relocation directory is not a real directory"
            ),
            Err(e) => return Err(e.into()),
        };
        #[cfg(unix)]
        File::open(&parent)?.sync_all()?;
        parent = next;
    }
    Ok(())
}
fn verify(path: &Path, length: u64, checksum: &str) -> Result<()> {
    let mut file = File::open(path)?;
    ensure!(
        file.metadata()?.len() == length,
        "relocation length mismatch"
    );
    let mut buffer = [0u8; 64 * 1024];
    let mut hash = blake3::Hasher::new();
    let mut count = 0;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        count += n as u64;
        ensure!(count <= length, "relocation source grew");
        hash.update(&buffer[..n]);
    }
    ensure!(
        count == length && hash.finalize().to_hex().as_str() == checksum,
        "relocation checksum mismatch"
    );
    Ok(())
}
impl PreviewStore {
    pub(super) fn recover_relocation_lock(&mut self) -> Result<()> {
        let value: Option<(String, String, String, String)> = self
            .db
            .query_row(
                "SELECT tier,source,target,phase FROM relocations LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        if let Some((tier, source, target, phase)) = value {
            let tier = match tier.as_str() {
                "thumbnail" => Tier::Thumbnail,
                "large" => Tier::Large,
                _ => bail!("invalid relocation tier"),
            };
            let extra = if phase == "copy" {
                target
            } else {
                ensure!(phase == "cleanup", "invalid relocation phase");
                source
            };
            self._relocation_lock = Some(lock_root(
                Path::new(&extra),
                &self.identity,
                tier,
                self.config.layout,
            )?);
        }
        Ok(())
    }

    pub fn relocation_pending(&self) -> Result<bool> {
        Ok(self
            .db
            .query_row("SELECT EXISTS(SELECT 1 FROM relocations)", [], |r| r.get(0))?)
    }
    /// Changing quotas never evicts a retained thumbnail. A lower retained budget
    /// simply prevents new publication until the owner supplies sufficient space.
    pub fn set_budgets(&mut self, thumbnail: u64, large: u64) -> Result<()> {
        ensure!(
            thumbnail > 0 && large > 0 && thumbnail <= i64::MAX as u64 && large <= i64::MAX as u64,
            "invalid cache budgets"
        );
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("INSERT INTO budgets VALUES('thumbnail',?1) ON CONFLICT(tier) DO UPDATE SET bytes=excluded.bytes",[thumbnail as i64])?;
        tx.execute("INSERT INTO budgets VALUES('large',?1) ON CONFLICT(tier) DO UPDATE SET bytes=excluded.bytes",[large as i64])?;
        tx.commit()?;
        self.config.thumbnail_bytes = thumbnail;
        self.config.large_bytes = large;
        Ok(())
    }
    pub fn begin_relocation(
        &mut self,
        tier: Tier,
        destination: &Path,
        original_roots: &[PathBuf],
    ) -> Result<()> {
        self.begin_relocation_inner(tier, destination, original_roots, || Ok(()))
    }
    fn begin_relocation_inner(
        &mut self,
        tier: Tier,
        destination: &Path,
        original_roots: &[PathBuf],
        after_markers: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        ensure!(
            !self.relocation_pending()?,
            "another cache relocation is in progress"
        );
        let unfinished: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM objects WHERE status!='ready')",
            [],
            |r| r.get(0),
        )?;
        ensure!(
            !unfinished,
            "recover pending/orphan publications before relocation"
        );
        self.flush_touches()?;
        let destination = prospective(destination)?;
        for other in [
            &self.config.manifest_root,
            &self.config.thumbnail_root,
            &self.config.large_root,
        ] {
            ensure!(
                !destination.starts_with(other) && !other.starts_with(&destination),
                "cache relocation roots overlap"
            );
        }
        for source in original_roots {
            let source = prospective(source)?;
            ensure!(
                !destination.starts_with(&source) && !source.starts_with(&destination),
                "relocation overlaps originals"
            );
        }
        fs::create_dir_all(&destination)?;
        // A crash after durable ownership markers but before the journal insert
        // can be retried only by this same manifest/tier/layout. No data objects
        // or foreign entries may be adopted. The marker lock below authenticates
        // ownership before an incomplete relocation marker is rewritten.
        for entry in fs::read_dir(&destination)? {
            let entry = entry?;
            ensure!(
                entry.file_type()?.is_file()
                    && matches!(
                        entry.file_name().to_str(),
                        Some(".photocatalog-preview-owner" | ".photocatalog-relocation")
                    ),
                "relocation destination contains non-admission files"
            );
        }
        let relocation_marker = destination.join(".photocatalog-relocation");
        ensure!(
            !relocation_marker.exists() || destination.join(".photocatalog-preview-owner").exists(),
            "relocation marker has no owning manifest"
        );
        if relocation_marker.exists() {
            ensure!(
                fs::metadata(destination.join(".photocatalog-preview-owner"))?.len() > 0,
                "relocation admission has no recorded owner identity"
            );
        }
        let target_lock = lock_root(&destination, &self.identity, tier, self.config.layout)?;
        let id = if relocation_marker.exists() {
            ensure!(
                fs::metadata(&relocation_marker)?.len() <= 36,
                "invalid relocation admission marker"
            );
            let mut bytes = [0; 37];
            let count = File::open(&relocation_marker)?.read(&mut bytes)?;
            std::str::from_utf8(&bytes[..count])
                .ok()
                .and_then(|value| uuid::Uuid::parse_str(value).ok())
                .unwrap_or_else(uuid::Uuid::new_v4)
                .to_string()
        } else {
            uuid::Uuid::new_v4().to_string()
        };
        let mut marker = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(relocation_marker)?;
        marker.write_all(id.as_bytes())?;
        marker.sync_all()?;
        #[cfg(unix)]
        File::open(&destination)?.sync_all()?;
        after_markers()?;
        self.db.execute(
            "INSERT INTO relocations VALUES(?1,?2,?3,?4,'copy','')",
            params![
                tier.name(),
                id,
                self.root(tier).to_str().context("cache path encoding")?,
                destination.to_str().context("cache path encoding")?
            ],
        )?;
        self._relocation_lock = Some(target_lock);
        Ok(())
    }
    /// At most `limit` entries and `byte_limit` bytes per call; an individual
    /// oversized object requires explicitly increasing the call's byte allowance.
    pub fn relocation_step(
        &mut self,
        tier: Tier,
        limit: usize,
        byte_limit: u64,
    ) -> Result<RelocationProgress> {
        ensure!(
            (1..=1024).contains(&limit) && byte_limit > 0,
            "relocation batch admission"
        );
        let row: Option<(String, String, String, String, String)> = self
            .db
            .query_row(
                "SELECT id,source,target,phase,cursor FROM relocations WHERE tier=?1",
                [tier.name()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        let Some((id, source, target, phase, cursor)) = row else {
            return Ok(RelocationProgress {
                tier,
                phase: "complete".into(),
                processed: 0,
                object_bytes: 0,
                complete: true,
            });
        };
        let source = PathBuf::from(source);
        let target = PathBuf::from(target);
        let mut marker_file = File::open(target.join(".photocatalog-relocation"))?;
        ensure!(
            id.len() <= 128 && marker_file.metadata()?.len() == id.len() as u64,
            "relocation marker length"
        );
        let mut marker = [0u8; 128];
        marker_file.read_exact(&mut marker[..id.len()])?;
        let mut extra = [0];
        ensure!(
            &marker[..id.len()] == id.as_bytes() && marker_file.read(&mut extra)? == 0,
            "relocation destination ownership changed"
        );
        let rows=self.db.prepare("SELECT key,bytes,checksum FROM objects WHERE tier=?1 AND status='ready' AND key>?2 ORDER BY key LIMIT ?3")?.query_map(params![tier.name(),cursor,limit as i64],|r|Ok((r.get::<_,String>(0)?,unsigned(r,1)?,r.get::<_,String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut progress = RelocationProgress {
            tier,
            phase: phase.clone(),
            processed: 0,
            object_bytes: 0,
            complete: false,
        };
        for (key, length, checksum) in &rows {
            if *length > byte_limit - progress.object_bytes {
                ensure!(
                    progress.processed > 0,
                    "object exceeds relocation byte allowance"
                );
                break;
            }
            let old = object_path(&source, self.config.layout, key)?;
            let new = object_path(&target, self.config.layout, key)?;
            if phase == "copy" {
                if new.exists() {
                    verify(&new, *length, checksum)?;
                } else {
                    ensure_destination_parent(&target, self.config.layout, key)?;
                    let temporary = new.with_extension("relocation-pending");
                    if temporary.exists() {
                        ensure!(
                            fs::symlink_metadata(&temporary)?.file_type().is_file(),
                            "invalid relocation temporary file"
                        );
                        fs::remove_file(&temporary)?;
                    }
                    // This deterministic path belongs to the marker-protected job;
                    // restart replaces only its own incomplete copy.
                    let mut output = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&temporary)?;
                    let mut input = File::open(&old)?;
                    let mut hash = blake3::Hasher::new();
                    let mut buffer = [0u8; 64 * 1024];
                    let mut copied = 0;
                    loop {
                        let n = input.read(&mut buffer)?;
                        if n == 0 {
                            break;
                        }
                        copied += n as u64;
                        ensure!(copied <= *length, "relocation source grew");
                        output.write_all(&buffer[..n])?;
                        hash.update(&buffer[..n]);
                    }
                    ensure!(
                        copied == *length && hash.finalize().to_hex().as_str() == checksum,
                        "relocation source checksum mismatch"
                    );
                    output.sync_all()?;
                    drop(output);
                    fs::hard_link(&temporary, &new)
                        .context("publish relocation copy without replacing an existing file")?;
                    fs::remove_file(&temporary)?;
                    #[cfg(unix)]
                    File::open(new.parent().unwrap())?.sync_all()?;
                }
            } else {
                ensure!(phase == "cleanup", "unknown relocation phase");
                // The authoritative copy is rechecked before removing the old one.
                verify(&new, *length, checksum)?;
                if old.exists() {
                    verify(&old, *length, checksum)?;
                    fs::remove_file(&old)?;
                }
            }
            self.db.execute(
                "UPDATE relocations SET cursor=?1 WHERE tier=?2",
                params![key, tier.name()],
            )?;
            progress.processed += 1;
            progress.object_bytes += *length;
        }
        if rows.len() < limit && progress.processed == rows.len() {
            if phase == "copy" {
                let tx = self
                    .db
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                tx.execute(
                    "UPDATE locations SET path=?1 WHERE tier=?2",
                    params![target.to_str().context("cache encoding")?, tier.name()],
                )?;
                tx.execute(
                    "UPDATE relocations SET phase='cleanup',cursor='' WHERE tier=?1",
                    [tier.name()],
                )?;
                tx.commit()?;
                let index = if tier == Tier::Thumbnail { 0 } else { 1 };
                let target_lock = self
                    ._relocation_lock
                    .take()
                    .context("relocation target lock missing")?;
                self._relocation_lock =
                    Some(std::mem::replace(&mut self._tier_locks[index], target_lock));
                match tier {
                    Tier::Thumbnail => self.config.thumbnail_root = target,
                    Tier::Large => self.config.large_root = target,
                };
                progress.phase = "cleanup".into();
            } else {
                self.db
                    .execute("DELETE FROM relocations WHERE tier=?1", [tier.name()])?;
                self._relocation_lock.take();
                progress.phase = "complete".into();
                progress.complete = true;
                // Keep the small ownership marker as an audit anchor. Foreign
                // files/empty old directories are never recursively removed.
            }
        }
        Ok(progress)
    }
    /// Resolve authoritative locations after a relocation committed but the app's
    /// settings file was not yet updated. The supplied roots initialize new stores.
    pub fn current_configuration(mut config: StoreConfig) -> Result<StoreConfig> {
        let path = config.manifest_root.join("previews.sqlite3");
        if !path.exists() {
            return Ok(config);
        }
        let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        ensure!(
            db.pragma_query_value(None, "application_id", |r| r.get::<_, i64>(0))? == 0x50435056,
            "not a preview manifest"
        );
        for tier in [Tier::Thumbnail, Tier::Large] {
            let path: String = db.query_row(
                "SELECT path FROM locations WHERE tier=?1",
                [tier.name()],
                |r| r.get(0),
            )?;
            match tier {
                Tier::Thumbnail => config.thumbnail_root = path.into(),
                Tier::Large => config.large_root = path.into(),
            };
        }
        let has_budgets: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='budgets')",
            [],
            |r| r.get(0),
        )?;
        if has_budgets {
            config.thumbnail_bytes = db.query_row(
                "SELECT bytes FROM budgets WHERE tier='thumbnail'",
                [],
                |r| unsigned(r, 0),
            )?;
            config.large_bytes =
                db.query_row("SELECT bytes FROM budgets WHERE tier='large'", [], |r| {
                    unsigned(r, 0)
                })?;
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interrupted_marker_admission_can_retry_only_owned_empty_target() {
        let root = tempfile::tempdir().unwrap();
        let config = StoreConfig {
            manifest_root: root.path().join("manifest"),
            thumbnail_root: root.path().join("thumb"),
            large_root: root.path().join("large"),
            layout: Layout::HashPrefix,
            thumbnail_bytes: 1000,
            large_bytes: 1000,
        };
        let destination = root.path().join("destination");
        let mut store = PreviewStore::open(config.clone(), &[]).unwrap();
        let source_identity =
            fs::read(config.thumbnail_root.join(".photocatalog-preview-owner")).unwrap();
        assert!(
            store
                .begin_relocation_inner(Tier::Thumbnail, &destination, &[], || {
                    anyhow::bail!("injected interruption after markers before journal")
                })
                .is_err()
        );
        assert!(!store.relocation_pending().unwrap());
        assert!(destination.join(".photocatalog-relocation").is_file());
        drop(store);
        let mut store = PreviewStore::open(config.clone(), &[]).unwrap();
        let foreign = destination.join("foreign.txt");
        fs::write(&foreign, b"preserve").unwrap();
        assert!(
            store
                .begin_relocation(Tier::Thumbnail, &destination, &[])
                .is_err()
        );
        assert_eq!(fs::read(&foreign).unwrap(), b"preserve");
        fs::remove_file(&foreign).unwrap(); // remove only the test-owned fixture
        store
            .begin_relocation(Tier::Thumbnail, &destination, &[])
            .unwrap();
        assert!(store.relocation_pending().unwrap());
        assert_eq!(
            fs::read(config.thumbnail_root.join(".photocatalog-preview-owner")).unwrap(),
            source_identity
        );
        drop(store);
        let mut store = PreviewStore::open(config.clone(), &[]).unwrap();
        while !store
            .relocation_step(Tier::Thumbnail, 10, 1000)
            .unwrap()
            .complete
        {}
        assert_eq!(
            store.config.thumbnail_root,
            fs::canonicalize(destination).unwrap()
        );
        assert!(config.thumbnail_root.exists());
    }
    #[test]
    fn another_manifests_admission_markers_are_never_adopted() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("foreign");
        fs::create_dir(&destination).unwrap();
        let lock = lock_root(
            &destination,
            "foreign-manifest",
            Tier::Thumbnail,
            Layout::Flat,
        )
        .unwrap();
        drop(lock);
        let before = fs::read(destination.join(".photocatalog-preview-owner")).unwrap();
        let config = StoreConfig {
            manifest_root: root.path().join("manifest"),
            thumbnail_root: root.path().join("thumb"),
            large_root: root.path().join("large"),
            layout: Layout::Flat,
            thumbnail_bytes: 100,
            large_bytes: 100,
        };
        let mut store = PreviewStore::open(config, &[]).unwrap();
        assert!(
            store
                .begin_relocation(Tier::Thumbnail, &destination, &[])
                .is_err()
        );
        assert_eq!(
            fs::read(destination.join(".photocatalog-preview-owner")).unwrap(),
            before
        );
        assert!(!destination.join(".photocatalog-relocation").exists());
    }
}

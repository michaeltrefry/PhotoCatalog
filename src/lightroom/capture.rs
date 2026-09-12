use super::{
    Issue, Limits, PROTOCOL, json_digest,
    source::{Revision, Source, reject_links},
    wal::{self, WalReport},
    write_new_json,
};
use crate::storage_volume::NativePath;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub source: NativePath,
    pub output: NativePath,
    pub include_auxiliary: bool,
    /// An explicit human/process-control assertion, never inferred from a process list.
    pub closed_application_evidence: Option<String>,
    pub limits: Limits,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Artifact {
    pub source: NativePath,
    pub role: String,
    pub relative: NativePath,
    pub stored: String,
    pub revision: Revision,
    pub blake3: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub protocol: u32,
    pub request: Request,
    pub state: String,
    pub raw_byte_retention: String,
    pub sqlite_consistency: String,
    pub application_consistency: String,
    pub cooperative_lock_protocol: String,
    pub artifacts: Vec<Artifact>,
    pub companion_inventory: Vec<Entry>,
    pub absent_companions: Vec<NativePath>,
    pub issues: Vec<Issue>,
    pub wal: Option<WalReport>,
    pub logical_blake3: Option<String>,
    pub logical_revision: Option<Revision>,
    pub revision_id: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub path: NativePath,
    pub role: String,
    pub relative: NativePath,
    pub directory: bool,
    pub modified_ns: Option<u128>,
    pub changed: String,
}
/// Run capture in a dedicated executable process. No original is opened by this
/// parent API; byte locks cannot be lost by an unrelated original FD close here.
pub fn spawn(executable: &Path, request: &Request) -> Result<Manifest> {
    let mut child = Command::new(executable)
        .arg("capture-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().context("capture worker stdin")?;
    serde_json::to_writer(&mut stdin, request)?;
    stdin.flush()?;
    drop(stdin);
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success(),
        "capture worker failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}
fn append(path: &Path, suffix: &str) -> PathBuf {
    let mut p = path.as_os_str().to_os_string();
    p.push(suffix);
    p.into()
}
fn companions(source: &Path) -> Vec<(PathBuf, &'static str)> {
    let stem = source.with_extension("");
    vec![
        (source.into(), "main"),
        (append(source, "-wal"), "wal"),
        (append(source, "-shm"), "shm"),
        (append(source, "-journal"), "journal"),
        (append(source, ".lock"), "lightroom_lock"),
        (append(&stem, ".lock"), "lightroom_lock"),
        (append(&stem, ".lrcat-data"), "auxiliary"),
        (append(&stem, ".acr"), "auxiliary"),
    ]
}
fn inventory(source: &Path, limits: &Limits) -> Result<(Vec<Entry>, Vec<NativePath>)> {
    let mut entries = vec![];
    let mut absent = vec![];
    let mut total = 0u64;
    let mut pending = vec![];
    for (path, role) in companions(source) {
        match fs::symlink_metadata(&path) {
            Ok(_) => pending.push((
                path.clone(),
                role,
                path.file_name()
                    .map(PathBuf::from)
                    .context("companion name")?,
                0usize,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                absent.push(NativePath::from_path(&path))
            }
            Err(e) => return Err(e.into()),
        }
    }
    while let Some((path, role, relative, depth)) = pending.pop() {
        reject_links(&path)?;
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            metadata.is_dir() || metadata.is_file(),
            "special companion node: {}",
            path.display()
        );
        entries.push(Entry {
            path: NativePath::from_path(&path),
            role: role.into(),
            relative: NativePath::from_path(&relative),
            directory: metadata.is_dir(),
            modified_ns: metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos()),
            changed: {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    format!(
                        "{}:{}:{}:{}",
                        metadata.dev(),
                        metadata.ino(),
                        metadata.ctime(),
                        metadata.ctime_nsec()
                    )
                }
                #[cfg(windows)]
                {
                    format!("{:?}:{}", metadata.created().ok(), metadata.len())
                }
            },
        });
        ensure!(
            entries.len() <= limits.max_files,
            "companion entry limit exceeded"
        );
        if metadata.is_dir() {
            ensure!(depth < limits.max_depth, "companion depth limit exceeded");
            for item in fs::read_dir(&path)? {
                ensure!(
                    entries.len() + pending.len() < limits.max_files,
                    "companion entry limit exceeded during enumeration"
                );
                let item = item?;
                pending.push((
                    item.path(),
                    role,
                    relative.join(item.file_name()),
                    depth + 1,
                ));
            }
        } else {
            ensure!(
                metadata.len() <= limits.max_file_bytes,
                "companion file byte limit exceeded"
            );
            total = total
                .checked_add(metadata.len())
                .context("companion byte count overflow")?;
            ensure!(
                total <= limits.max_total_bytes,
                "companion total byte limit exceeded"
            );
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    absent.sort();
    Ok((entries, absent))
}
fn validate_output(source: &Path, output: &Path) -> Result<PathBuf> {
    ensure!(!output.exists(), "capture output must not exist");
    let filename = output
        .file_name()
        .context("capture output needs a directory name")?;
    ensure!(filename != "." && filename != "..", "invalid output name");
    let parent = fs::canonicalize(output.parent().context("output parent required")?)?;
    let output = parent.join(filename);
    let source_parent = source.parent().context("source parent required")?;
    ensure!(
        !output.starts_with(source_parent) && !source_parent.starts_with(&output),
        "source and capture output directories must be disjoint"
    );
    Ok(output)
}
/// Only the dedicated `capture-worker` process should call this entry point.
/// Do not call inside a process with any SQLite connection to original inodes.
pub fn run_isolated(request: Request) -> Result<Manifest> {
    run_observed(request, |_, _, _| Ok(()))
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Publication {
    ReferencesDurable,
    PendingDurable,
    Published,
}
fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(windows)]
    let _ = path;
    Ok(())
}
fn run_observed(
    mut request: Request,
    mut observe: impl FnMut(Publication, &Path, &Manifest) -> Result<()>,
) -> Result<Manifest> {
    request.limits.validate()?;
    let source = request.source.to_path()?;
    reject_links(&source)?;
    let source = fs::canonicalize(source)?;
    ensure!(source.is_file(), "catalog source must be a regular file");
    let output = validate_output(&source, &request.output.to_path()?)?;
    request.source = NativePath::from_path(&source);
    request.output = NativePath::from_path(&output);
    let builder = fs::DirBuilder::new();
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = builder;
        builder.mode(0o700);
        builder
    };
    builder.create(&output)?;
    let mut report=Manifest {protocol:PROTOCOL,request,state:"pending".into(),raw_byte_retention:"incomplete".into(),sqlite_consistency:"unverified".into(),application_consistency:"unverified".into(),cooperative_lock_protocol:"default SQLite main shared [1073741824,1073742336); SHM shared DMS 128 then [120,128); alternate VFS/proprietary locks unverified".into(),artifacts:vec![],companion_inventory:vec![],absent_companions:vec![],issues:vec![],wal:None,logical_blake3:None,logical_revision:None,revision_id:None};
    write_new_json(&output.join("request.json"), &report.request)?;
    let result = capture(&source, &output, &mut report);
    if let Err(error) = result {
        report.state = "failed".into();
        report
            .issues
            .push(Issue::new("capture_failed", format!("{error:#}")));
    }
    if let Err(error) = super::bounded_json(&report, super::MANIFEST_BYTES - 1) {
        let counts = (report.artifacts.len(), report.companion_inventory.len());
        report.state = "failed".into();
        report.raw_byte_retention = "incomplete_manifest".into();
        report.revision_id = None;
        report.artifacts.clear();
        report.companion_inventory.clear();
        report.absent_companions.clear();
        report.issues = vec![Issue::new(
            "manifest_limit",
            format!(
                "{error}; unpublished raw files remain; artifact/inventory counts were {counts:?}. No complete capture manifest was published."
            ),
        )];
    }
    // All referenced files have already been flushed by capture(). Sync their
    // names before a complete manifest can become visible to another process.
    sync_directory(&output)?;
    observe(Publication::ReferencesDurable, &output, &report)?;
    let pending = output.join("manifest.pending.json");
    write_new_json(&pending, &report)?;
    sync_directory(&output)?;
    observe(Publication::PendingDurable, &output, &report)?;
    // Unlike rename(), hard-link creation never replaces an unexpected target.
    // Same-directory publication is atomic; unsupported filesystems fail closed
    // and preserve the pending file for explicit interrupted-capture diagnosis.
    fs::hard_link(&pending, output.join("manifest.json"))?;
    fs::remove_file(&pending)?;
    sync_directory(&output)?;
    observe(Publication::Published, &output, &report)?;
    Ok(report)
}
fn capture(source: &Path, output: &Path, report: &mut Manifest) -> Result<()> {
    let limits = &report.request.limits;
    let (entries, absent) = inventory(source, limits)?;
    report.companion_inventory = entries.clone();
    report.absent_companions = absent.clone();
    ensure!(
        !entries.iter().any(|e| e.role == "lightroom_lock"),
        "Lightroom lock marker is present; close the application before capture"
    );
    let mut handles = BTreeMap::new();
    let mut identities = BTreeSet::new();
    for (index, entry) in entries.iter().enumerate().filter(|(_, e)| !e.directory) {
        if entry.role == "auxiliary" && !report.request.include_auxiliary {
            continue;
        }
        let handle = Source::open(&entry.path.to_path()?, limits.max_file_bytes)?;
        ensure!(
            identities.insert(handle.before.object.clone()),
            "duplicate source inode aliases are unsafe with process-scoped locks"
        );
        handles.insert(index, handle);
    }
    let role_index = |role: &str| entries.iter().position(|e| e.role == role && !e.directory);
    let main = role_index("main").context("main catalog absent")?;
    handles
        .get_mut(&main)
        .context("main handle")?
        .lock(0x4000_0000, 512)?;
    if let Some(index) = role_index("shm") {
        let handle = handles.get_mut(&index).context("SHM handle")?;
        handle.lock(128, 1)?; // Must precede ordinary WAL locks: prevents fresh-opener truncation.
        handle.lock(120, 8)?;
    }
    if let Some(index) = role_index("wal") {
        ensure!(
            handles[&index].before.bytes == 0 || role_index("shm").is_some(),
            "nonempty WAL without lockable SHM: unsafe source capture"
        );
    }
    let aux_dirs: Vec<_> = entries
        .iter()
        .filter(|e| e.role == "auxiliary" && e.directory)
        .filter_map(|e| e.path.to_path().ok())
        .collect();
    for directory in &aux_dirs {
        let lock = directory.join("LOCK");
        let recognizable =
            directory.join("CURRENT").is_file() && directory.join("IDENTITY").is_file();
        if recognizable {
            let index = entries
                .iter()
                .position(|e| e.path == NativePath::from_path(&lock));
            if report.request.include_auxiliary {
                let index =
                    index.context("recognized auxiliary layout has no existing LOCK file")?;
                handles
                    .get_mut(&index)
                    .context("auxiliary LOCK handle")?
                    .lock(0, 0)?;
            }
            report.issues.push(Issue::new("auxiliary_lock_evidence","CURRENT/IDENTITY/LOCK layout observed; POSIX whole-file shared barrier is cooperative evidence, not Adobe engine/version or cross-store transaction proof"));
        }
    }
    // Re-inventory after locking to catch a writer's create/delete between initial discovery and locks.
    ensure!(
        inventory(source, limits)? == (entries.clone(), absent.clone()),
        "companion set changed while acquiring locks"
    );
    fs::create_dir(output.join("raw"))?;
    for (index, handle) in &mut handles {
        let entry = &entries[*index];
        let stored = format!("raw/{index:06}.bin");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output.join(&stored))?;
        let hash = handle.copy_and_hash(Some(&mut file))?;
        file.sync_all()?;
        report.artifacts.push(Artifact {
            source: entry.path.clone(),
            role: entry.role.clone(),
            relative: entry.relative.clone(),
            stored,
            revision: handle.before.clone(),
            blake3: hash,
        });
    }
    for (index, handle) in &mut handles {
        let artifact = report
            .artifacts
            .iter()
            .find(|a| a.source == entries[*index].path)
            .context("artifact record")?;
        ensure!(
            handle.copy_and_hash(None)? == artifact.blake3,
            "source content changed between digest passes"
        );
    }
    ensure!(
        inventory(source, limits)? == (entries, absent),
        "companion membership changed during capture"
    );
    for handle in handles.values() {
        handle.verify()?;
    }
    #[cfg(unix)]
    File::open(output.join("raw"))?.sync_all()?;
    drop(handles); // No subsequent live source operation. All SQLite work is private.
    let omitted = report
        .companion_inventory
        .iter()
        .any(|e| e.role == "auxiliary")
        && !report.request.include_auxiliary;
    report.raw_byte_retention = if omitted {
        "auxiliary_omitted_for_discovery"
    } else {
        "complete"
    }
    .into();
    if omitted {
        report.issues.push(Issue::new(
            "auxiliary_not_captured",
            "Main-only family discovery is not a complete preservation capture",
        ));
    }
    if let Some(artifact) = report.artifacts.iter().find(|a| a.role == "journal")
        && artifact.revision.bytes > 0
    {
        anyhow::bail!(
            "nonempty rollback journal retained; hot/cold status cannot be asserted safely"
        );
    }
    if let Some(artifact) = report.artifacts.iter().find(|a| a.role == "wal") {
        let wal = wal::validate(&output.join(&artifact.stored))?;
        let torn = wal.trailing_bytes != 0;
        report.wal = Some(wal);
        ensure!(
            !torn,
            "torn WAL tail retained; complete logical consistency withheld"
        );
    }
    let work = output.join("working");
    fs::create_dir(&work)?;
    for artifact in &report.artifacts {
        let name = match artifact.role.as_str() {
            "main" => "catalog.sqlite3",
            "wal" => "catalog.sqlite3-wal",
            _ => continue,
        };
        fs::copy(output.join(&artifact.stored), work.join(name))?;
    }
    // The retained SHM is evidence, not an input to private recovery. SQLite
    // rebuilds a fresh private SHM from the validated retained main/WAL pair.
    super::plan::recover_private(
        &work.join("catalog.sqlite3"),
        &output.join("logical.sqlite3"),
        limits,
    )?;
    let mut logical = Source::open(&output.join("logical.sqlite3"), limits.max_total_bytes)?;
    report.logical_blake3 = Some(logical.copy_and_hash(None)?);
    report.logical_revision = Some(logical.before.clone());
    report.sqlite_consistency = "consistent_default_sqlite".into();
    report.application_consistency = if report
        .request
        .closed_application_evidence
        .as_ref()
        .is_some_and(|s| !s.trim().is_empty())
        && !omitted
    {
        "closed_application_assertion_recorded"
    } else {
        "unverified"
    }
    .into();
    report.revision_id = Some(json_digest(&report.artifacts)?);
    report.state = "captured".into();
    Ok(())
}
pub fn read_manifest(directory: &Path) -> Result<Manifest> {
    let mut bytes = vec![];
    File::open(directory.join("manifest.json"))?
        .take(super::MANIFEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= super::MANIFEST_BYTES,
        "manifest limit exceeded"
    );
    let report: Manifest = serde_json::from_slice(&bytes)?;
    ensure!(report.protocol == PROTOCOL, "unknown capture protocol");
    Ok(report)
}

#[cfg(test)]
mod publication_tests {
    use super::*;
    fn fixture(root: &Path, name: &str) -> Request {
        let source_dir = root.join(format!("source-{name}"));
        fs::create_dir(&source_dir).unwrap();
        let source = source_dir.join("fixture.lrcat");
        let db = rusqlite::Connection::open(&source).unwrap();
        db.execute_batch("CREATE TABLE opaque(v); INSERT INTO opaque VALUES(x'00ff');")
            .unwrap();
        drop(db);
        Request {
            source: NativePath::from_path(&source),
            output: NativePath::from_path(&root.join(format!("capture-{name}"))),
            include_auxiliary: true,
            closed_application_evidence: None,
            limits: Limits::default(),
        }
    }
    fn verify_references(root: &Path, manifest: &Manifest) {
        assert_eq!(manifest.state, "captured");
        for artifact in &manifest.artifacts {
            assert_eq!(
                super::super::digest(&fs::read(root.join(&artifact.stored)).unwrap()),
                artifact.blake3
            );
        }
        assert_eq!(
            Some(super::super::digest(
                &fs::read(root.join("logical.sqlite3")).unwrap()
            )),
            manifest.logical_blake3
        );
    }
    #[test]
    fn complete_manifest_is_published_only_after_durable_references_and_pending_json() {
        let temp = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let mut observed = vec![];
        run_observed(fixture(temp.path(), "complete"), |stage, root, manifest| {
            observed.push(stage);
            verify_references(root, manifest);
            match stage {
                Publication::ReferencesDurable => {
                    assert!(!root.join("manifest.json").exists());
                    assert!(!root.join("manifest.pending.json").exists());
                }
                Publication::PendingDurable => {
                    assert!(read_manifest(root).is_err());
                    let pending: Manifest =
                        serde_json::from_slice(&fs::read(root.join("manifest.pending.json"))?)
                            .unwrap();
                    verify_references(root, &pending);
                }
                Publication::Published => {
                    verify_references(root, &read_manifest(root)?);
                    assert!(!root.join("manifest.pending.json").exists());
                }
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(
            observed,
            vec![
                Publication::ReferencesDurable,
                Publication::PendingDurable,
                Publication::Published
            ]
        );
    }
    #[test]
    fn interrupted_publication_retains_pending_evidence_and_never_clobbers_a_destination() {
        let temp = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        for collision in [false, true] {
            let request = fixture(
                temp.path(),
                if collision {
                    "collision"
                } else {
                    "interrupted"
                },
            );
            let output = request.output.to_path().unwrap();
            let result = run_observed(request, |stage, root, _| {
                if stage == Publication::PendingDurable {
                    if collision {
                        fs::write(root.join("manifest.json"), b"unexpected existing file")?;
                    } else {
                        anyhow::bail!("injected interruption before publication");
                    }
                }
                Ok(())
            });
            assert!(result.is_err());
            assert!(output.join("manifest.pending.json").is_file());
            if collision {
                assert_eq!(
                    fs::read(output.join("manifest.json")).unwrap(),
                    b"unexpected existing file"
                );
            } else {
                assert!(!output.join("manifest.json").exists());
            }
            assert!(read_manifest(&output).is_err());
        }
    }
}

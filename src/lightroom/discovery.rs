use super::{Issue, Limits, source::reject_links};
use crate::storage_volume::NativePath;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, fs, path::Path, time::UNIX_EPOCH};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candidate {
    pub path: NativePath,
    pub bytes: u64,
    pub modified_ns: Option<u128>,
    pub filename_hint: Option<String>,
    pub version_hint: Option<u32>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Inventory {
    pub protocol: u32,
    pub root: NativePath,
    pub candidates: Vec<Candidate>,
    pub exclusions: Vec<NativePath>,
    pub issues: Vec<Issue>,
    pub complete: bool,
    pub entries: usize,
}
/// Filename-only grouping is explicitly provisional; it is not lineage or recency.
pub fn filename_hint(stem: &str) -> (String, Option<u32>) {
    let mut name = stem.to_owned();
    let mut version = None;
    while let Some(index) = name.rfind("-v") {
        let suffix = &name[index + 2..];
        let mut parts = suffix.split('-');
        let first = parts.next().unwrap_or_default();
        if first.is_empty()
            || !first.bytes().all(|b| b.is_ascii_digit())
            || !parts.all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        {
            break;
        }
        version = version.or_else(|| first.parse().ok());
        name.truncate(index);
    }
    (name, version)
}
pub fn discover(root: &Path, limits: &Limits) -> Result<Inventory> {
    discover_controlled(root, limits, None, || Ok(()))
}
pub(crate) fn discover_controlled(
    root: &Path,
    limits: &Limits,
    admission: Option<(usize, usize)>,
    mut check: impl FnMut() -> Result<()>,
) -> Result<Inventory> {
    check()?;
    limits.validate()?;
    reject_links(root)?;
    let root = fs::canonicalize(root)?;
    ensure!(root.is_dir(), "catalog discovery root is not a directory");
    let mut result = Inventory {
        protocol: super::PROTOCOL,
        root: NativePath::from_path(&root),
        candidates: vec![],
        exclusions: vec![],
        issues: vec![],
        complete: true,
        entries: 0,
    };
    let mut retained = 0usize;
    let mut admit = |path: &Path| -> Result<()> {
        if let Some((bytes, units)) = admission {
            let native = NativePath::from_path(path);
            let n = match &native {
                NativePath::UnixBytes(v) => v.len(),
                NativePath::WindowsWide(v) => v.len(),
            };
            ensure!(n <= units, "discovery native path admission exceeded");
            // Charge every visited path before any pending/seen/result copies;
            // covers path arrays, escaped hints, and issue/object overhead.
            let charge = n
                .checked_mul(32)
                .and_then(|n| n.checked_add(2048))
                .ok_or_else(|| anyhow::anyhow!("discovery byte admission overflow"))?;
            retained = retained
                .checked_add(charge)
                .ok_or_else(|| anyhow::anyhow!("discovery byte admission overflow"))?;
            ensure!(
                retained <= bytes,
                "discovery aggregate byte admission exceeded; use a narrower explicit root or larger result budget"
            );
        }
        Ok(())
    };
    admit(&root)?;
    let mut pending = vec![(root, 0)];
    let mut seen = BTreeSet::new();
    while let Some((directory, depth)) = pending.pop() {
        check()?;
        if !seen.insert(directory.clone()) {
            continue;
        }
        let entries = match fs::read_dir(&directory) {
            Ok(v) => v,
            Err(e) => {
                result.issues.push(Issue::new(
                    "unreadable_directory",
                    format!("{}: {e}", directory.display()),
                ));
                result.complete = false;
                continue;
            }
        };
        for entry in entries {
            check()?;
            let entry = match entry {
                Ok(v) => v,
                Err(e) => {
                    admit(&directory)?;
                    result
                        .issues
                        .push(Issue::new("unreadable_entry", e.to_string()));
                    result.complete = false;
                    continue;
                }
            };
            result.entries += 1;
            if result.entries > limits.max_files {
                result.issues.push(Issue::new(
                    "inventory_limit",
                    "directory entry limit reached",
                ));
                result.complete = false;
                return Ok(result);
            }
            let path = entry.path();
            admit(&path)?;
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                result.exclusions.push(NativePath::from_path(&path));
                result
                    .issues
                    .push(Issue::new("link_excluded", path.display().to_string()));
                continue;
            }
            if kind.is_dir() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.ends_with(".lrdata")
                    || name.ends_with(".lrcat-data")
                    || name.ends_with(".acr")
                    || name.eq_ignore_ascii_case("backups")
                    || name.eq_ignore_ascii_case("old lightroom catalogs")
                {
                    result.exclusions.push(NativePath::from_path(&path));
                    continue;
                }
                if depth >= limits.max_depth {
                    result
                        .issues
                        .push(Issue::new("depth_limit", path.display().to_string()));
                    result.complete = false;
                } else {
                    ensure!(
                        pending.len() < limits.max_files,
                        "discovery pending directory limit exceeded"
                    );
                    pending.push((path, depth + 1));
                }
            } else if kind.is_file()
                && path
                    .extension()
                    .is_some_and(|v| v.eq_ignore_ascii_case("lrcat"))
            {
                let metadata = entry.metadata()?;
                let hint = path.file_stem().and_then(|s| s.to_str()).map(filename_hint);
                result.candidates.push(Candidate {
                    path: NativePath::from_path(&path),
                    bytes: metadata.len(),
                    modified_ns: metadata
                        .modified()
                        .ok()
                        .and_then(|v| v.duration_since(UNIX_EPOCH).ok())
                        .map(|v| v.as_nanos()),
                    filename_hint: hint.as_ref().map(|v| v.0.clone()),
                    version_hint: hint.and_then(|v| v.1),
                });
            }
        }
    }
    result.candidates.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(result)
}

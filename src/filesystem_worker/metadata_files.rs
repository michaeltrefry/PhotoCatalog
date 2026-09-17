use crate::{
    application::U64,
    catalog_session::metadata_files::{
        Action, DiscoveryEntry, EvidenceReceipt, Mode, Reply, Request, Value,
    },
    storage_volume::NativePath,
};
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    io::Read,
    sync::atomic::{AtomicBool, Ordering},
};

struct Transfer {
    request: Request,
    mode: Mode,
    bytes: Vec<u8>,
    evidence: Option<(fs::File, std::path::PathBuf)>,
    hasher: blake3::Hasher,
    expected: u64,
    digest: String,
    offset: u64,
}
struct Discovery {
    transfer: crate::catalog_session::LeaseId,
    directory: NativePath,
    reader: fs::ReadDir,
    pending: Option<fs::DirEntry>,
    cursor: Option<NativePath>,
    exhausted: bool,
}
#[derive(Default)]
pub(super) struct Owner {
    transfer: Option<Transfer>,
    discovery: Option<Discovery>,
    terminal: Option<(Request, Reply)>,
}
impl Owner {
    pub fn empty(&self) -> bool {
        self.transfer.is_none() && self.discovery.is_none()
    }
    pub fn call(&mut self, request: &Request, cancel: &AtomicBool) -> Result<Reply> {
        request.validate()?;
        if let Some((old, reply)) = &self.terminal
            && old.transfer == request.transfer
            && old.operation == request.operation
        {
            ensure!(old == request, "metadata file replay differs");
            return Ok(reply.clone());
        }
        let value = match &request.action {
            Action::Begin {
                mode,
                bytes,
                blake3,
            } => {
                check(cancel)?;
                ensure!(
                    self.transfer.is_none(),
                    "metadata file transfer already active"
                );
                let capacity = usize::try_from(bytes.0)?;
                let mut payload = Vec::new();
                let evidence = if let Mode::Evidence { destination } = mode {
                    Some(crate::metadata_export::create_evidence_new(
                        &destination.to_path()?,
                    )?)
                } else {
                    payload.try_reserve_exact(capacity)?;
                    None
                };
                self.transfer = Some(Transfer {
                    request: request.clone(),
                    mode: mode.clone(),
                    bytes: payload,
                    evidence,
                    hasher: blake3::Hasher::new(),
                    expected: bytes.0,
                    digest: blake3.clone(),
                    offset: 0,
                });
                Value::Begun
            }
            Action::Append { offset, bytes } => {
                check(cancel)?;
                let transfer = self
                    .transfer
                    .as_mut()
                    .context("metadata file transfer is not active")?;
                ensure!(
                    transfer.request.transfer == request.transfer && transfer.offset == offset.0,
                    "metadata file upload offset"
                );
                ensure!(
                    transfer.offset.saturating_add(bytes.len() as u64) <= transfer.expected,
                    "metadata file upload extent"
                );
                if let Some((file, _)) = &mut transfer.evidence {
                    use std::io::Write;
                    file.write_all(bytes)?;
                } else {
                    transfer.bytes.extend_from_slice(bytes);
                }
                transfer.hasher.update(bytes);
                transfer.offset += bytes.len() as u64;
                Value::Appended {
                    offset: U64(transfer.offset),
                }
            }
            Action::Finish => {
                let transfer = self
                    .transfer
                    .take()
                    .context("metadata file transfer is not active")?;
                ensure!(
                    transfer.request.transfer == request.transfer
                        && transfer.offset == transfer.expected,
                    "metadata file upload incomplete"
                );
                ensure!(
                    transfer.hasher.finalize().to_hex().as_str() == transfer.digest,
                    "metadata file upload digest"
                );
                check(cancel)?;
                match transfer.mode {
                    Mode::Plan {
                        destination,
                        max_existing_bytes,
                        alias_limits,
                    } => {
                        let mut checkpoint = |_| {
                            if cancel.load(Ordering::Acquire) {
                                Err(std::io::Error::new(
                                    std::io::ErrorKind::Interrupted,
                                    "metadata planning canceled",
                                ))
                            } else {
                                Ok(())
                            }
                        };
                        Value::Plan(crate::metadata_export::plan_export_controlled(
                            &destination.to_path()?,
                            &transfer.bytes,
                            max_existing_bytes.0,
                            alias_limits,
                            &mut checkpoint,
                        )?)
                    }
                    Mode::Apply { plan } => {
                        Value::Receipt(crate::metadata_export::apply_export_with_hook(
                            &plan,
                            &transfer.bytes,
                            |_| check_io(cancel),
                        )?)
                    }
                    Mode::Recover { plan } => {
                        ensure!(transfer.bytes.is_empty(), "recovery payload must be empty");
                        Value::Receipt(crate::metadata_export::recover_export(&recovery(&plan))?)
                    }
                    Mode::Restore { plan } => {
                        ensure!(transfer.bytes.is_empty(), "restore payload must be empty");
                        Value::Receipt(crate::metadata_export::restore_planned_export(&plan)?)
                    }
                    Mode::Evidence { destination } => {
                        let (file, path) = transfer
                            .evidence
                            .context("metadata evidence file is not active")?;
                        crate::metadata_export::finish_evidence_new(&file, &path)?;
                        Value::Evidence(EvidenceReceipt {
                            destination,
                            bytes: U64(transfer.expected),
                            blake3: transfer.digest,
                        })
                    }
                    Mode::Existing {
                        plan,
                        offset,
                        length,
                    } => {
                        ensure!(
                            transfer.bytes.is_empty(),
                            "existing-file read payload must be empty"
                        );
                        let mut checkpoint = |_| check_io(cancel);
                        let bytes = crate::metadata_export::read_planned_existing_chunk(
                            &plan,
                            offset.0,
                            length as usize,
                            &mut checkpoint,
                        )?;
                        let expected = plan.expected.as_ref().unwrap();
                        Value::Existing {
                            offset,
                            total: U64(expected.bytes),
                            bytes,
                            blake3: expected.digest.clone(),
                        }
                    }
                }
            }
            Action::Release => {
                if self
                    .transfer
                    .as_ref()
                    .is_some_and(|value| value.request.transfer == request.transfer)
                {
                    self.transfer = None;
                }
                if self
                    .discovery
                    .as_ref()
                    .is_some_and(|value| value.transfer == request.transfer)
                {
                    self.discovery = None;
                }
                Value::Released
            }
            Action::Discover {
                directory,
                after,
                scan_rows,
                page_rows,
            } => self.discover(
                &request.transfer,
                directory,
                after.as_ref(),
                scan_rows.0,
                page_rows.0,
                cancel,
            )?,
        };
        let reply = Reply {
            root: request.root.clone(),
            transfer: request.transfer.clone(),
            operation: request.operation,
            value,
        };
        reply.validate(request)?;
        if matches!(
            request.action,
            Action::Finish | Action::Release | Action::Discover { .. }
        ) {
            self.terminal = Some((request.clone(), reply.clone()));
        }
        Ok(reply)
    }

    fn discover(
        &mut self,
        transfer: &crate::catalog_session::LeaseId,
        directory: &NativePath,
        after: Option<&NativePath>,
        scan_rows: u64,
        page_rows: u64,
        cancel: &AtomicBool,
    ) -> Result<Value> {
        check(cancel)?;
        if self.discovery.is_none() {
            ensure!(
                after.is_none(),
                "new metadata discovery requires an empty cursor"
            );
            let path = directory.to_path()?;
            ensure!(path.is_absolute(), "recovery directory must be absolute");
            let canonical = path.canonicalize()?;
            let canonical = NativePath::from_path(&canonical);
            ensure!(
                &canonical == directory,
                "recovery directory must be canonical"
            );
            self.discovery = Some(Discovery {
                transfer: transfer.clone(),
                directory: directory.clone(),
                reader: fs::read_dir(canonical.to_path()?)?,
                pending: None,
                cursor: None,
                exhausted: false,
            });
        }
        let discovery = self.discovery.as_mut().unwrap();
        ensure!(
            &discovery.transfer == transfer && &discovery.directory == directory,
            "metadata discovery authority changed"
        );
        ensure!(
            discovery.cursor.as_ref() == after,
            "metadata discovery cursor changed"
        );
        discovery_page(discovery, scan_rows, page_rows, cancel)
    }
}
fn discovery_page(
    discovery: &mut Discovery,
    scan_rows: u64,
    page_rows: u64,
    cancel: &AtomicBool,
) -> Result<Value> {
    let mut rows = Vec::new();
    let mut scanned = 0u64;
    while scanned < scan_rows && rows.len() < page_rows as usize && !discovery.exhausted {
        check(cancel)?;
        let entry = match discovery.pending.take() {
            Some(entry) => Some(Ok(entry)),
            None => discovery.reader.next(),
        };
        let Some(entry) = entry else {
            discovery.exhausted = true;
            break;
        };
        let entry = entry?;
        scanned += 1;
        let path = entry.path();
        discovery.cursor = Some(NativePath::from_path(&path));
        if entry.file_type()?.is_dir()
            && entry
                .file_name()
                .to_string_lossy()
                .starts_with(".photocatalog-xmp-export-")
        {
            rows.push(discovery_entry(path)?);
        }
    }
    if !discovery.exhausted && discovery.pending.is_none() {
        match discovery.reader.next() {
            Some(entry) => discovery.pending = Some(entry?),
            None => discovery.exhausted = true,
        }
    }
    Ok(Value::Discovery {
        rows,
        next: (!discovery.exhausted)
            .then(|| discovery.cursor.clone())
            .flatten(),
        scanned: U64(scanned),
    })
}
fn check(cancel: &AtomicBool) -> Result<()> {
    ensure!(
        !cancel.load(Ordering::Acquire),
        "metadata file operation canceled"
    );
    Ok(())
}
fn check_io(cancel: &AtomicBool) -> std::io::Result<()> {
    if cancel.load(Ordering::Acquire) {
        Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "metadata file operation canceled",
        ))
    } else {
        Ok(())
    }
}
fn recovery(plan: &crate::metadata_export::ExportPlan) -> std::path::PathBuf {
    plan.destination
        .parent()
        .unwrap()
        .join(format!(".photocatalog-xmp-export-{}", plan.operation))
}
fn discovery_entry(p: std::path::PathBuf) -> Result<DiscoveryEntry> {
    let name = NativePath::from_path(std::path::Path::new(
        p.file_name().context("recovery name")?,
    ));
    let mut kind = "unknown".to_owned();
    let mut operation = None;
    let mut plan_digest = None;
    let mut detail = "No complete plan is available; retained for inspection.".to_owned();
    if p.file_name()
        .unwrap()
        .to_string_lossy()
        .contains("-preparing-")
    {
        kind = "preparing".into();
        detail = "Preparation is incomplete; publication is unavailable.".into();
    } else if let Ok(file) = crate::metadata_export::open_regular(&p.join("plan.json")) {
        let mut bytes = Vec::new();
        file.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() <= 64 * 1024 {
            if let Ok(plan) = serde_json::from_slice::<crate::metadata_export::ExportPlan>(&bytes) {
                kind = "known".into();
                operation = Some(plan.operation);
                plan_digest = Some(blake3::hash(&bytes).to_hex().to_string());
                detail = "Complete recovery plan; action revalidates catalog authority.".into();
            } else {
                kind = "invalid".into();
            }
        }
    }
    Ok(DiscoveryEntry {
        directory: NativePath::from_path(&p),
        name,
        kind,
        operation,
        plan_digest,
        detail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(path: &std::path::Path) -> crate::catalog_session::RootCapability {
        #[cfg(unix)]
        let physical = crate::catalog_session::PhysicalObjectId::Unix {
            device: U64(1),
            inode: U64(2),
        };
        #[cfg(windows)]
        let physical = crate::catalog_session::PhysicalObjectId::Windows {
            volume_serial: U64(1),
            file_index: U64(2),
        };
        crate::catalog_session::RootCapability {
            epoch: crate::catalog_session::LeaseId::new(),
            token: crate::catalog_session::LeaseId::new(),
            session: crate::catalog_session::LeaseId::new(),
            canonical_root: NativePath::from_path(path),
            root_physical: physical,
            catalog_physical: physical,
        }
    }

    fn call(
        owner: &mut Owner,
        root: &crate::catalog_session::RootCapability,
        transfer: &crate::catalog_session::LeaseId,
        operation: u64,
        action: Action,
    ) -> Result<Value> {
        Ok(owner
            .call(
                &Request {
                    root: root.clone(),
                    transfer: transfer.clone(),
                    operation: U64(operation),
                    action,
                },
                &AtomicBool::new(false),
            )?
            .value)
    }

    #[test]
    fn sparse_discovery_keeps_iterator_progress_beyond_one_scan_window() -> Result<()> {
        let temp = tempfile::tempdir()?;
        for index in 0..12 {
            fs::write(temp.path().join(format!("ordinary-{index}")), b"x")?;
        }
        fs::create_dir(temp.path().join(".photocatalog-xmp-export-target"))?;
        let directory = NativePath::from_path(&temp.path().canonicalize()?);
        let mut discovery = Discovery {
            transfer: crate::catalog_session::LeaseId::new(),
            directory,
            reader: fs::read_dir(temp.path())?,
            pending: None,
            cursor: None,
            exhausted: false,
        };
        let cancel = AtomicBool::new(false);
        let mut found = false;
        let mut saw_sparse_continuation = false;
        for _ in 0..20 {
            let Value::Discovery { rows, next, .. } =
                discovery_page(&mut discovery, 1, 1, &cancel)?
            else {
                unreachable!()
            };
            found |= rows
                .iter()
                .any(|row| row.operation.as_deref() == Some("target") || row.kind == "unknown");
            saw_sparse_continuation |= rows.is_empty() && next.is_some();
            if next.is_none() {
                break;
            }
        }
        assert!(saw_sparse_continuation);
        assert!(found);
        Ok(())
    }

    #[test]
    fn filesystem_owner_plans_applies_and_restores_without_losing_original() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let canonical = temp.path().canonicalize()?;
        let root = root(&canonical);
        let destination = canonical.join("sidecar.xmp");
        fs::write(&destination, b"original sidecar")?;
        let payload = b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"/>".to_vec();
        let digest = blake3::hash(&payload).to_hex().to_string();
        let mut owner = Owner::default();

        let planning = crate::catalog_session::LeaseId::new();
        assert!(matches!(
            call(
                &mut owner,
                &root,
                &planning,
                1,
                Action::Begin {
                    mode: Mode::Plan {
                        destination: NativePath::from_path(&destination),
                        max_existing_bytes: U64(1024),
                        alias_limits: Default::default(),
                    },
                    bytes: U64(payload.len() as u64),
                    blake3: digest.clone(),
                },
            )?,
            Value::Begun
        ));
        call(
            &mut owner,
            &root,
            &planning,
            2,
            Action::Append {
                offset: U64(0),
                bytes: payload.clone(),
            },
        )?;
        let Value::Plan(plan) = call(&mut owner, &root, &planning, 3, Action::Finish)? else {
            unreachable!()
        };

        let existing = crate::catalog_session::LeaseId::new();
        call(
            &mut owner,
            &root,
            &existing,
            1,
            Action::Begin {
                mode: Mode::Existing {
                    plan: plan.clone(),
                    offset: U64(1),
                    length: 4,
                },
                bytes: U64(0),
                blake3: blake3::hash(&[]).to_hex().to_string(),
            },
        )?;
        let Value::Existing {
            offset,
            total,
            bytes,
            blake3,
        } = call(&mut owner, &root, &existing, 2, Action::Finish)?
        else {
            unreachable!()
        };
        assert_eq!(offset, U64(1));
        assert_eq!(total, U64(b"original sidecar".len() as u64));
        assert_eq!(bytes, b"rigi");
        assert_eq!(blake3, plan.expected.as_ref().unwrap().digest);

        let applying = crate::catalog_session::LeaseId::new();
        call(
            &mut owner,
            &root,
            &applying,
            4,
            Action::Begin {
                mode: Mode::Apply { plan: plan.clone() },
                bytes: U64(payload.len() as u64),
                blake3: digest,
            },
        )?;
        call(
            &mut owner,
            &root,
            &applying,
            5,
            Action::Append {
                offset: U64(0),
                bytes: payload.clone(),
            },
        )?;
        let Value::Receipt(applied) = call(&mut owner, &root, &applying, 6, Action::Finish)? else {
            unreachable!()
        };
        assert_eq!(
            applied.state,
            crate::metadata_export::ExportState::Published
        );
        let captured = applied.captured_original.context("captured original")?;
        assert_eq!(fs::read(&captured)?, b"original sidecar");
        assert_eq!(fs::read(&destination)?, payload);

        fs::remove_file(&destination)?;
        let restoring = crate::catalog_session::LeaseId::new();
        call(
            &mut owner,
            &root,
            &restoring,
            7,
            Action::Begin {
                mode: Mode::Restore { plan: plan.clone() },
                bytes: U64(0),
                blake3: blake3::hash(&[]).to_hex().to_string(),
            },
        )?;
        let Value::Receipt(restored) = call(&mut owner, &root, &restoring, 8, Action::Finish)?
        else {
            unreachable!()
        };
        assert_eq!(
            restored.state,
            crate::metadata_export::ExportState::Restored
        );
        assert_eq!(restored.captured_original.as_ref(), Some(&captured));
        crate::metadata_export::validate_metadata_export_receipt_wire(&restored, &plan)?;
        assert_eq!(fs::read(&captured)?, b"original sidecar");
        assert_eq!(fs::read(destination)?, b"original sidecar");
        Ok(())
    }

    #[test]
    fn evidence_stream_writes_chunks_without_accumulating_document() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let canonical = temp.path().canonicalize()?;
        let root = root(&canonical);
        let destination = canonical.join("evidence.json");
        let payload = vec![b'x'; crate::catalog_session::metadata_files::CHUNK_BYTES + 37];
        let digest = blake3::hash(&payload).to_hex().to_string();
        let transfer = crate::catalog_session::LeaseId::new();
        let mut owner = Owner::default();
        call(
            &mut owner,
            &root,
            &transfer,
            1,
            Action::Begin {
                mode: Mode::Evidence {
                    destination: NativePath::from_path(&destination),
                },
                bytes: U64(payload.len() as u64),
                blake3: digest,
            },
        )?;
        call(
            &mut owner,
            &root,
            &transfer,
            2,
            Action::Append {
                offset: U64(0),
                bytes: payload[..crate::catalog_session::metadata_files::CHUNK_BYTES].to_vec(),
            },
        )?;
        assert!(owner.transfer.as_ref().unwrap().bytes.is_empty());
        call(
            &mut owner,
            &root,
            &transfer,
            3,
            Action::Append {
                offset: U64(crate::catalog_session::metadata_files::CHUNK_BYTES as u64),
                bytes: payload[crate::catalog_session::metadata_files::CHUNK_BYTES..].to_vec(),
            },
        )?;
        let Value::Evidence(receipt) = call(&mut owner, &root, &transfer, 4, Action::Finish)?
        else {
            unreachable!()
        };
        assert_eq!(receipt.bytes.0, payload.len() as u64);
        assert_eq!(fs::read(destination)?, payload);
        Ok(())
    }
}

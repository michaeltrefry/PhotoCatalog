use anyhow::{Result, ensure};
use photocatalog::{Catalog, catalog_backup::Limits};
use serde_json::Value;
use std::{
    ffi::OsStr,
    fs,
    io::{BufRead, BufReader},
    path::Path,
    process::{Command, Stdio},
    sync::mpsc,
    time::Duration,
};

fn invoke(root: &Path, args: &[&OsStr]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_photocatalog"))
        .arg("--catalog")
        .arg(root)
        .args(args)
        .output()
        .expect("run backup CLI")
}

fn success(output: std::process::Output) -> Result<Value> {
    ensure!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn limits_file(parent: &Path) -> Result<std::path::PathBuf> {
    let path = parent.join("limits.json");
    fs::write(&path, serde_json::to_vec(&Limits::default())?)?;
    Ok(path)
}

#[test]
fn cli_roundtrip_uses_read_only_inspection_and_explicit_restore_release() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source");
    drop(Catalog::open(&source)?);
    let before = fs::read(source.join("catalog.sqlite3"))?;
    let unused = temp.path().join("must not be created");
    let bundle = temp.path().join("backup ü");
    let restored = temp.path().join("restored");
    let limits = limits_file(temp.path())?;
    let options = [OsStr::new("--limits"), limits.as_os_str()];

    let template = success(invoke(&unused, &[OsStr::new("backup-limits")]))?;
    let _: Limits = serde_json::from_value(template)?;
    assert!(!unused.exists());
    let backup = success(invoke(
        &source,
        &[
            OsStr::new("backup-create"),
            bundle.as_os_str(),
            options[0],
            options[1],
        ],
    ))?;
    assert_eq!(before, fs::read(source.join("catalog.sqlite3"))?);
    let inspected = success(invoke(
        &unused,
        &[
            OsStr::new("backup-inspect"),
            bundle.as_os_str(),
            options[0],
            options[1],
        ],
    ))?;
    assert_eq!(backup, inspected);
    assert!(!unused.exists());
    assert!(Catalog::open(&bundle).is_err());

    let receipt = success(invoke(
        &restored,
        &[
            OsStr::new("backup-restore"),
            bundle.as_os_str(),
            options[0],
            options[1],
        ],
    ))?;
    assert_eq!(receipt["backup"], backup);
    let id = receipt["restore_id"].as_str().unwrap();
    let held = success(invoke(&restored, &[OsStr::new("restore-status")]))?;
    assert_eq!(held["jobs_held"], true);
    assert!(
        !invoke(&restored, &[OsStr::new("restore-resume"), OsStr::new(id)])
            .status
            .success()
    );
    assert!(
        !invoke(
            &restored,
            &[
                OsStr::new("restore-resume"),
                OsStr::new("wrong-receipt"),
                OsStr::new("--acknowledge-pending-jobs"),
            ],
        )
        .status
        .success()
    );
    assert_eq!(
        success(invoke(&restored, &[OsStr::new("restore-status")]))?["jobs_held"],
        true
    );
    let resumed = success(invoke(
        &restored,
        &[
            OsStr::new("restore-resume"),
            OsStr::new(id),
            OsStr::new("--acknowledge-pending-jobs"),
        ],
    ))?;
    assert_eq!(resumed["jobs_held"], false);
    assert!(Catalog::open(&restored)?.browse(0, 1)?.is_empty());
    Ok(())
}

#[test]
fn cli_rejects_missing_source_bad_limits_and_preexisting_cancellation() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let absent = temp.path().join("absent");
    let bundle = temp.path().join("backup");
    let limits = limits_file(temp.path())?;
    let args = [
        OsStr::new("backup-create"),
        bundle.as_os_str(),
        OsStr::new("--limits"),
        limits.as_os_str(),
    ];
    assert!(!invoke(&absent, &args).status.success());
    assert!(!absent.exists(), "backup preflight initialized its source");
    assert!(!bundle.exists());

    drop(Catalog::open(&absent)?);
    let before = fs::read(absent.join("catalog.sqlite3"))?;
    let cancel = temp.path().join("cancel");
    fs::write(&cancel, [])?;
    let mut cancelled_args = args.to_vec();
    cancelled_args.extend([OsStr::new("--cancel-file"), cancel.as_os_str()]);
    let cancelled = invoke(&absent, &cancelled_args);
    assert!(!cancelled.status.success());
    assert!(String::from_utf8_lossy(&cancelled.stderr).contains("cancelled"));
    assert!(!bundle.exists());

    let mut invalid = serde_json::to_value(Limits::default())?;
    invalid["misspelled_limit"] = 1.into();
    fs::write(&limits, serde_json::to_vec(&invalid)?)?;
    assert!(!invoke(&absent, &args).status.success());
    assert!(!bundle.exists());
    assert_eq!(before, fs::read(absent.join("catalog.sqlite3"))?);
    Ok(())
}

fn kill_at_copy(root: &Path, command: &str, bundle: &Path, limits: &Path) -> Result<()> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_photocatalog"))
        .arg("--catalog")
        .arg(root)
        .arg(command)
        .arg(bundle)
        .arg("--limits")
        .arg(limits)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let stderr = child.stderr.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut transcript = String::new();
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            if transcript.len() < 64 * 1024 {
                transcript.push_str(&line);
                transcript.push('\n');
            }
            if serde_json::from_str::<Value>(&line)
                .ok()
                .is_some_and(|value| value["phase"] == "Copy")
            {
                let _ = sender.send(());
            }
        }
        transcript
    });
    // Always kill/reap and drain the pipe, including timeout or early child failure.
    let observed_copy = receiver.recv_timeout(Duration::from_secs(30));
    let killed = child.kill();
    let status = child.wait();
    let transcript = reader.join().unwrap();
    ensure!(observed_copy.is_ok(), "no copy checkpoint: {transcript}");
    killed?;
    ensure!(!status?.success(), "child completed before interruption");
    Ok(())
}

#[test]
fn terminated_backup_and_restore_leave_prior_catalog_and_bundle_usable() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source");
    drop(Catalog::open(&source)?);
    {
        let db = rusqlite::Connection::open(source.join("catalog.sqlite3"))?;
        // Padding makes the actual child copy observable. Domain preservation is
        // independently exercised by the real metadata and migration fixtures.
        db.execute_batch("CREATE TABLE interruption_padding(bytes BLOB NOT NULL)")?;
        db.execute(
            "INSERT INTO interruption_padding VALUES(zeroblob(?1))",
            [64 * 1024 * 1024],
        )?;
    }
    let before = blake3::hash(&fs::read(source.join("catalog.sqlite3"))?);
    let configured = Limits {
        pages_per_step: 1,
        max_seconds: 60,
        ..Limits::default()
    };
    let limits = limits_file(temp.path())?;
    fs::write(&limits, serde_json::to_vec(&configured)?)?;
    let good = temp.path().join("complete-backup");
    let receipt =
        photocatalog::catalog_backup::backup_catalog(&source, &good, &configured, |_| Ok(()))?;

    let interrupted = temp.path().join("interrupted-backup");
    kill_at_copy(&source, "backup-create", &interrupted, &limits)?;
    assert!(interrupted.join(".photocatalog-pending.json").exists());
    assert!(
        photocatalog::catalog_backup::inspect_backup(&interrupted, &configured, |_| Ok(()))
            .is_err()
    );
    assert!(Catalog::open(&interrupted).is_err());

    let restored = temp.path().join("interrupted-restore");
    kill_at_copy(&restored, "backup-restore", &good, &limits)?;
    assert!(restored.join(".photocatalog-pending.json").exists());
    assert!(Catalog::open(&restored).is_err());
    assert_eq!(
        before,
        blake3::hash(&fs::read(source.join("catalog.sqlite3"))?)
    );
    assert!(Catalog::open(&source)?.browse(0, 1)?.is_empty());
    assert_eq!(
        receipt,
        photocatalog::catalog_backup::inspect_backup(&good, &configured, |_| Ok(()))?
    );
    Ok(())
}

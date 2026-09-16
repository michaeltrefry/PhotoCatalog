use super::*;
use crate::{lightroom::Limits, storage_volume::NativePath};
use std::{
    fs,
    time::{Duration, Instant},
};

fn request(root: &Path) -> Request {
    Request {
        source: NativePath::from_path(&root.join("source.lrcat")),
        output: NativePath::from_path(&root.join("captured")),
        include_auxiliary: true,
        closed_application_evidence: Some("synthetic fixture; no Lightroom application".into()),
        limits: Limits::default(),
    }
}

fn command(mode: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--ignored",
            "--exact",
            "lightroom::capture::process::tests::isolated_process_fixture",
            "--nocapture",
        ])
        .env("PHOTOCATALOG_CAPTURE_TEST_MODE", mode);
    command
}

fn until(mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !check() {
        assert!(Instant::now() < deadline, "capture fixture timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

// Invoked only by the test process owner, never by the installed worker.
#[test]
#[ignore]
fn isolated_process_fixture() {
    let mode = std::env::var("PHOTOCATALOG_CAPTURE_TEST_MODE").unwrap();
    if mode == "tamper" {
        OpenOptions::new()
            .append(true)
            .open("request.json")
            .unwrap()
            .write_all(b" ")
            .unwrap();
    }
    if mode == "oversize" || mode == "truncated" {
        // Both channels exceed ordinary pipe capacity. The owner uses null
        // endpoints, so neither buffering nor pipe draining is required.
        let noise = [b'x'; 65536];
        for _ in 0..4 {
            std::io::stdout().write_all(&noise).unwrap();
            std::io::stderr().write_all(&noise).unwrap();
        }
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open("result.json")
            .unwrap();
        if mode == "oversize" {
            file.set_len(RESULT_BYTES as u64 + 1).unwrap();
        } else {
            (&file).write_all(b"{").unwrap();
        }
        return;
    }
    worker_main_with(|request| {
        if mode == "error" {
            bail!("{}", "界".repeat(100_000));
        }
        let source = request.source.to_path()?;
        let mut held = Source::open(&source, 1024)?;
        held.lock(0x4000_0000, 512)?;
        let output = request.output.to_path()?;
        fs::create_dir(&output)?;
        write_new(&output.join("partial.raw"), b"retained interrupted fixture")?;
        write_new(Path::new("ready"), b"held")?;
        loop {
            std::thread::sleep(Duration::from_millis(10));
        }
    })
    .unwrap();
}

#[test]
fn cancel_drop_and_lease_eof_reap_owned_child_and_retain_partial_evidence() {
    for action in ["cancel", "drop", "eof"] {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let request = request(&root);
        fs::write(request.source.to_path().unwrap(), b"synthetic source bytes").unwrap();
        let mut process = CaptureProcess::spawn_command(command("hold"), &root, &request).unwrap();
        let staging = process.staging_directory().to_path_buf();
        until(|| staging.join("ready").exists());
        assert!(process.poll().unwrap().is_none());
        let pid = process.pid();
        #[cfg(unix)]
        {
            // A GUI's unrelated source FD close must not release the child's
            // process-scoped source lock. This is deliberately outside the API.
            drop(std::fs::File::open(request.source.to_path().unwrap()).unwrap());
            use std::os::fd::AsRawFd;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(request.source.to_path().unwrap())
                .unwrap();
            let mut lock: libc::flock = unsafe { std::mem::zeroed() };
            lock.l_type = libc::F_WRLCK as _;
            lock.l_whence = libc::SEEK_SET as _;
            lock.l_start = 0x4000_0000;
            lock.l_len = 512;
            assert_eq!(
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut lock) },
                0
            );
            assert_ne!(lock.l_type, libc::F_UNLCK as libc::c_short);
            assert_eq!(lock.l_pid as u32, pid);
        }
        match action {
            "cancel" => {
                process.cancel_and_wait().unwrap();
                assert!(process.exited);
                drop(process);
            }
            "drop" => drop(process),
            "eof" => {
                process.lease.take();
                let mut terminal = None;
                until(|| match process.poll() {
                    Ok(None) => false,
                    other => {
                        terminal = Some(other);
                        true
                    }
                });
                assert!(terminal.unwrap().is_err());
                assert!(process.exited);
                drop(process);
            }
            _ => unreachable!(),
        }
        #[cfg(unix)]
        {
            assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
        #[cfg(windows)]
        let _ = pid;
        assert_eq!(
            fs::read(request.source.to_path().unwrap()).unwrap(),
            b"synthetic source bytes"
        );
        assert_eq!(
            fs::read(request.output.to_path().unwrap().join("partial.raw")).unwrap(),
            b"retained interrupted fixture"
        );
        assert!(staging.join("request.json").exists());
        assert!(
            !request
                .output
                .to_path()
                .unwrap()
                .join("manifest.json")
                .exists()
        );
        if action != "eof" {
            assert!(staging.join("termination.json").exists());
        }
    }
}

#[test]
fn bounded_transport_errors_do_not_wait_for_stdout_and_retain_diagnostics() {
    for mode in ["oversize", "truncated", "error", "tamper"] {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let mut process =
            CaptureProcess::spawn_command(command(mode), &root, &request(&root)).unwrap();
        let staging = process.staging_directory().to_path_buf();
        let mut result = None;
        until(|| match process.poll() {
            Ok(None) => false,
            other => {
                result = Some(other);
                true
            }
        });
        assert!(result.unwrap().is_err());
        assert!(process.exited);
        assert!(staging.join("request.json").exists());
        assert!(!root.join("captured").exists());
        if mode == "tamper" {
            let text: String =
                serde_json::from_slice(&fs::read(staging.join("error.json")).unwrap()).unwrap();
            assert!(text.contains("request changed before worker admission"));
        }
        if mode == "error" {
            let bytes = fs::read(staging.join("error.json")).unwrap();
            assert!(bytes.len() <= ERROR_BYTES);
            let text: String = serde_json::from_slice(&bytes).unwrap();
            assert!(text.len() <= 4096);
            assert!(text.contains('界'));
        }
    }
}

#[test]
fn preadmission_bounds_and_native_paths_preserve_request_authority() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let mut request = request(&root);
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        request.source = NativePath::from_path(
            &root.join(std::ffi::OsString::from_vec(b"source-\xff.lrcat".to_vec())),
        );
    }
    let bytes = bounded_json(&request, MANIFEST_BYTES).unwrap();
    let roundtrip: Request = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(roundtrip.source, request.source);
    assert_eq!(
        roundtrip.closed_application_evidence,
        request.closed_application_evidence
    );
    request.closed_application_evidence = Some("x".repeat(MANIFEST_BYTES));
    assert!(CaptureProcess::spawn_command(command("hold"), &root, &request).is_err());
    assert_eq!(
        fs::read_dir(&root).unwrap().count(),
        0,
        "admission must precede transport/process creation"
    );
    let path = root.join("large");
    fs::write(&path, b"12345").unwrap();
    assert!(read_bounded(&path, 4).is_err());
    assert_eq!(read_bounded(&path, 5).unwrap(), b"12345");
    #[cfg(unix)]
    {
        let link = root.join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read_bounded(&link, 100).is_err());
    }
}

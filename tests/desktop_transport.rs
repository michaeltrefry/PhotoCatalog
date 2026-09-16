//! Actual owned child proof is deliberately Status/Close only. No catalog/native worker is opened.
use photocatalog::{
    application::{
        Config, Limits, Reply, Request, Response,
        desktop::{DesktopBridge, TransportPhase},
    },
    preview,
};

#[test]
fn installed_dispatch_protocol_status_close_and_wait_without_catalog_or_decoder()
-> anyhow::Result<()> {
    let executable = assert_cmd::cargo::cargo_bin!("photocatalog").to_path_buf();
    let bridge = DesktopBridge::spawn(Config {
        worker_executable: executable,
        cache_root: None,
        original_roots: vec![],
        preview_policy: preview::PreviewPolicy::default(),
        preview_limits: preview::ServiceLimits::default(),
        limits: Limits::default(),
    })?;
    let pid = bridge.status().pid;
    assert!(pid > 0);
    assert_eq!(bridge.status().phase, TransportPhase::Ready);
    // A caller may hold a reply indefinitely; the control reader must keep progressing.
    let held = bridge.submit(Request::Status)?;
    for _ in 0..8 {
        assert!(matches!(
            bridge.submit(Request::Status)?.recv(),
            Reply::Ok {
                value: Response::Status(_)
            }
        ));
    }
    assert!(matches!(
        bridge
            .submit(Request::Close {
                catalog: "never-opened".into()
            })?
            .recv(),
        Reply::Error { .. }
    ));
    assert!(matches!(
        held.recv(),
        Reply::Ok {
            value: Response::Status(_)
        }
    ));
    bridge.try_shutdown()?;
    assert_eq!(bridge.status().phase, TransportPhase::Closed);
    assert!(bridge.submit(Request::Status).is_err());
    bridge.try_shutdown()?;
    #[cfg(unix)]
    {
        // Only the PID returned by this owned child, after wait, is probed.
        assert_eq!(unsafe { libc::kill(pid as libc::pid_t, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
    Ok(())
}

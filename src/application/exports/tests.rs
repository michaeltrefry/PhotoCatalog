use super::*;
use crate::application::{Bridge, Reply, Request as AppRequest, Response as AppResponse};
use crate::image_export::{AlphaPolicy, IntegerDepth, OutputFormat, OutputSize};
use std::sync::atomic::{AtomicBool, Ordering};
fn fixture() -> anyhow::Result<(tempfile::TempDir, Catalog)> {
    let temp = tempfile::tempdir()?;
    let original = temp.path().join("original-é.png");
    std::fs::write(&original, b"original custody bytes")?;
    let mut c = Catalog::open(temp.path().join("catalog"))?;
    let fingerprint = blake3::hash(b"original custody bytes").to_hex().to_string();
    c.db.execute("INSERT INTO assets(id,location,path_display,state,fingerprint,preview_hash,metadata) VALUES('a',?1,?2,'ready',?3,'fixture','{\"format\":\"PNG\",\"width\":32,\"height\":24,\"orientation\":1,\"camera_make\":null,\"camera_model\":null,\"captured_at\":null,\"preview_source\":\"fixture\"}')",rusqlite::params![crate::location_bytes(&original),original.to_string_lossy(),fingerprint])?;
    c.record_storage_path("a", &NativePath::from_path(&original))?;
    Ok((temp, c))
}
fn config() -> Config {
    Config {
        worker_executable: std::env::current_exe().unwrap(),
        cache_root: None,
        original_roots: vec![],
        preview_policy: Default::default(),
        preview_limits: Default::default(),
        limits: Default::default(),
        import_checkpoint: None,
    }
}
fn output() -> Output {
    Output {
        size: OutputSize::Original,
        format: OutputFormat::Png {
            depth: IntegerDepth::Sixteen,
        },
        profile: Profile::Srgb,
        alpha: AlphaPolicy::Preserve,
    }
}
fn call(bridge: &Bridge, request: AppRequest) -> anyhow::Result<AppResponse> {
    let pending = bridge.submit(request)?;
    match pending.receiver.recv_timeout(Duration::from_secs(5))? {
        Reply::Ok { value } => Ok(value),
        Reply::Error { error } => Err(error.into()),
    }
}
fn export(bridge: &Bridge, token: &str, request: Request) -> anyhow::Result<Response> {
    match call(
        bridge,
        AppRequest::Export {
            catalog: token.into(),
            request: Box::new(request),
        },
    )? {
        AppResponse::Export(v) => Ok(*v),
        _ => anyhow::bail!("wrong export envelope"),
    }
}
fn terminal(bridge: &Bridge, token: &str, id: &str) -> anyhow::Result<Operation> {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let Response::Operation(Some(op)) = export(
            bridge,
            token,
            Request::Status {
                operation: Some(id.into()),
            },
        )?
        else {
            anyhow::bail!("no operation")
        };
        if ["complete", "failed", "canceled", "paused"].contains(&op.phase.as_str()) {
            return Ok(op);
        }
        anyhow::ensure!(
            Instant::now() < until,
            "operation did not terminate: {op:?}"
        );
        thread::sleep(Duration::from_millis(5));
    }
}
#[test]
fn exact_decimal_wire_and_all_output_choices() -> anyhow::Result<()> {
    for value in [0, u64::MAX as u128, u128::MAX] {
        let text = serde_json::to_string(&U128(value))?;
        assert_eq!(serde_json::from_str::<U128>(&text)?, U128(value));
    }
    for text in ["1", "\"01\"", "\"-1\"", "\"+1\""] {
        assert!(serde_json::from_str::<U128>(text).is_err());
    }
    for format in [
        OutputFormat::Jpeg { quality: 1 },
        OutputFormat::Jpeg { quality: 100 },
        OutputFormat::Png {
            depth: IntegerDepth::Eight,
        },
        OutputFormat::Png {
            depth: IntegerDepth::Sixteen,
        },
        OutputFormat::Tiff {
            depth: crate::image_export::TiffDepth::Eight,
        },
        OutputFormat::Tiff {
            depth: crate::image_export::TiffDepth::Sixteen,
        },
        OutputFormat::Tiff {
            depth: crate::image_export::TiffDepth::Float32,
        },
    ] {
        let value = Request::Append {
            job: "job".into(),
            expected_total: I64(9_007_199_254_740_999),
            target: Target {
                key: VariantKey::master("a"),
                expected_revision: I64(9_007_199_254_740_999),
                destination: NativePath::UnixBytes(vec![47, 255]),
                overwrite: true,
                metadata: Metadata::Resolved {
                    expected_revision: I64(i64::MAX),
                    base_model: Some(I64(i64::MAX)),
                },
            },
            output: Output { format, ..output() },
            budgets: None,
        };
        let text = serde_json::to_string(&value)?;
        assert!(text.contains("\"9007199254740999\""));
        assert_eq!(
            serde_json::to_string(&serde_json::from_str::<Request>(&text)?)?,
            text
        );
    }
    Ok(())
}
#[test]
fn bounded_job_pages_and_exact_frozen_plan_chunks() -> anyhow::Result<()> {
    let (temp, mut c) = fixture()?;
    let high = 9_007_199_254_740_999i64;
    for i in 0..3 {
        c.db.execute("INSERT INTO photo_export_jobs(sequence,id,state,total,completed) VALUES(?1,?2,'building',0,0)",rusqlite::params![high+i,format!("job{i}")])?;
    }
    let config = config();
    let cache = Mutex::new(Cache::default());
    let opts = worker::options(&config);
    let Response::Jobs { rows, next } = read::execute(
        &c,
        Request::Jobs {
            after: I64(0),
            limit: U64(2),
        },
        &config.limits,
        &cache,
        &opts,
    )?
    else {
        panic!()
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(next, Some(I64(high + 1)));
    let core_output = crate::image_export::OutputSpec {
        size: OutputSize::Original,
        format: output().format,
        profile: crate::image_export::OutputProfile::Srgb,
        alpha: AlphaPolicy::Preserve,
    };
    c.append_photo_export(
        "job0",
        0,
        &core::ExportTarget {
            key: VariantKey::master("a"),
            expected_revision: 0,
            destination: temp.path().join("export-é.png"),
            overwrite: false,
            metadata: core::MetadataSelection::Omit,
        },
        &core_output,
        1024,
        1024,
    )?;
    let (raw, authority) = read::document(&c, "job0", 1)?;
    let mut collected = String::new();
    let mut offset = U64(0);
    loop {
        let Response::PlanChunk(chunk) = read::execute(
            &c,
            Request::PlanChunk {
                job: "job0".into(),
                sequence: I64(1),
                authority: authority.clone(),
                offset,
                bytes: U64(17),
            },
            &config.limits,
            &cache,
            &opts,
        )?
        else {
            panic!()
        };
        collected.push_str(&chunk.text);
        match chunk.next {
            Some(next) => offset = next,
            None => break,
        }
    }
    assert_eq!(collected, raw.raw());
    assert!(
        read::execute(
            &c,
            Request::PlanChunk {
                job: "job0".into(),
                sequence: I64(1),
                authority: "0".repeat(64),
                offset: U64(0),
                bytes: U64(17)
            },
            &config.limits,
            &cache,
            &opts
        )
        .is_err()
    );
    c.db.execute(
        "UPDATE photo_export_items SET plan=zeroblob(131073) WHERE job='job0'",
        [],
    )?;
    assert!(matches!(
        read::document(&c, "job0", 1).unwrap_err().code,
        ErrorCode::ResourceLimit
    ));
    assert!(c.photo_export_plan("job0", 1).is_err());
    Ok(())
}
#[test]
fn profile_tokens_are_rgb_validated_bounded_and_native_links_rejected() -> anyhow::Result<()> {
    let (temp, catalog) = fixture()?;
    let p = temp.path().join("output.icc");
    let bytes = lcms2::Profile::new_srgb().icc()?;
    std::fs::write(&p, &bytes)?;
    let ctx = worker::Context {
        control: Default::default(),
        cache: Default::default(),
        config: config(),
    };
    let ResultValue::Profile(info) = worker::profile(&ctx, &catalog, &NativePath::from_path(&p))?
    else {
        panic!()
    };
    assert_eq!(info.bytes, U64(bytes.len() as u64));
    assert!(!info.linear);
    let snapshot = ctx.cache.lock().unwrap().profiles[&info.token]
        .bytes
        .clone();
    ctx.cache.lock().unwrap().profiles.clear();
    let frozen = worker::output(
        &ctx,
        Output {
            profile: Profile::Icc {
                token: info.token.clone(),
            },
            ..output()
        },
        Some(snapshot),
    )?;
    assert!(
        matches!(frozen.profile,crate::image_export::OutputProfile::Icc{bytes:ref b} if *b==bytes)
    );
    assert!(
        worker::output(
            &ctx,
            Output {
                profile: Profile::Icc { token: info.token },
                ..output()
            },
            None
        )
        .is_err()
    );
    std::fs::write(&p, b"not an ICC")?;
    assert!(worker::profile(&ctx, &catalog, &NativePath::from_path(&p)).is_err());
    #[cfg(unix)]
    {
        let link = temp.path().join("alias.icc");
        std::os::unix::fs::symlink(&p, &link)?;
        assert!(worker::profile(&ctx, &catalog, &NativePath::from_path(&link)).is_err());
    }
    Ok(())
}

#[test]
fn managed_profile_uses_authority_bytes_and_never_admits_partial_or_invalid_content()
-> anyhow::Result<()> {
    use crate::catalog_session::{ExportProfileAction, export_profile_managed_session};

    let temp = tempfile::tempdir()?;
    let bytes = lcms2::Profile::new_srgb().icc()?;
    let requested = NativePath::from_path(&temp.path().join("caller-profile-does-not-exist.icc"));
    let (mut session, requests) =
        export_profile_managed_session(temp.path(), bytes.clone(), false, false, false, false)?;
    let ctx = worker::Context {
        control: Default::default(),
        cache: Default::default(),
        config: config(),
    };
    let catalog = session.catalog.as_ref().unwrap();
    let ResultValue::Profile(info) = worker::profile(&ctx, catalog, &requested)? else {
        panic!("profile result")
    };
    assert_eq!(info.name, "caller-profile-does-not-exist.icc");
    assert_eq!(info.bytes, U64(bytes.len() as u64));
    assert_eq!(info.blake3, blake3::hash(&bytes).to_hex().as_str());
    assert!(!info.linear);
    assert_eq!(
        ctx.cache.lock().unwrap().profiles[&info.token]
            .bytes
            .as_slice(),
        bytes.as_slice()
    );
    let calls = requests.lock().unwrap();
    assert!(matches!(
        calls.first().unwrap().action,
        ExportProfileAction::Begin
    ));
    assert!(matches!(
        calls.last().unwrap().action,
        ExportProfileAction::Finish
    ));
    assert!(calls.iter().all(|r| r.requested == requested));
    drop(calls);
    session.close()?;

    let invalid = tempfile::tempdir()?;
    let (mut session, _) = export_profile_managed_session(
        invalid.path(),
        b"not an ICC".to_vec(),
        false,
        false,
        false,
        false,
    )?;
    let ctx = worker::Context {
        control: Default::default(),
        cache: Default::default(),
        config: config(),
    };
    assert!(worker::profile(&ctx, session.catalog.as_ref().unwrap(), &requested).is_err());
    assert!(ctx.cache.lock().unwrap().profiles.is_empty());
    session.close()?;

    let canceled = tempfile::tempdir()?;
    let (mut session, requests) = export_profile_managed_session(
        canceled.path(),
        vec![7; 32 * 1024],
        true,
        false,
        false,
        true,
    )?;
    let ctx = worker::Context {
        control: Default::default(),
        cache: Default::default(),
        config: config(),
    };
    assert!(worker::profile(&ctx, session.catalog.as_ref().unwrap(), &requested).is_err());
    assert!(ctx.cache.lock().unwrap().profiles.is_empty());
    assert!(matches!(
        requests.lock().unwrap().last().unwrap().action,
        ExportProfileAction::Abort
    ));
    session.close()?;

    let lost = tempfile::tempdir()?;
    let (mut session, requests) =
        export_profile_managed_session(lost.path(), vec![9; 32 * 1024], false, true, false, true)?;
    let ctx = worker::Context {
        control: Default::default(),
        cache: Default::default(),
        config: config(),
    };
    assert!(worker::profile(&ctx, session.catalog.as_ref().unwrap(), &requested).is_err());
    assert!(ctx.cache.lock().unwrap().profiles.is_empty());
    let requests = requests.lock().unwrap();
    assert!(matches!(
        requests.first().unwrap().action,
        ExportProfileAction::Begin
    ));
    assert!(matches!(
        requests.last().unwrap().action,
        ExportProfileAction::Abort
    ));
    drop(requests);
    session.close()?;

    let lost_finish = tempfile::tempdir()?;
    let (mut session, requests) =
        export_profile_managed_session(lost_finish.path(), bytes, false, false, true, true)?;
    let ctx = worker::Context {
        control: Default::default(),
        cache: Default::default(),
        config: config(),
    };
    assert!(worker::profile(&ctx, session.catalog.as_ref().unwrap(), &requested).is_err());
    assert!(ctx.cache.lock().unwrap().profiles.is_empty());
    let requests = requests.lock().unwrap();
    assert!(matches!(
        requests[requests.len() - 2].action,
        ExportProfileAction::Finish
    ));
    assert!(matches!(
        requests.last().unwrap().action,
        ExportProfileAction::Abort
    ));
    drop(requests);
    session.close()?;
    Ok(())
}
#[test]
fn actor_hold_keeps_reads_cancel_and_close_responsive_and_cancel_reply_is_job() -> anyhow::Result<()>
{
    let (temp, mut c) = fixture()?;
    let job = c.begin_photo_export()?;
    drop(c);
    let (entered, events) = mpsc::channel();
    let released = Arc::new(AtomicBool::new(false));
    let release = released.clone();
    let mut config = config();
    config.import_checkpoint = Some(Arc::new(move |stage, cancel| {
        if stage == "export_hold" {
            let _ = entered.send(());
            while !release.load(Ordering::Acquire) && !cancel.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(2));
            }
        }
    }));
    let bridge = Bridge::spawn(config)?;
    let AppResponse::Status(status) = call(
        &bridge,
        AppRequest::OpenExisting {
            path: NativePath::from_path(&temp.path().join("catalog")),
        },
    )?
    else {
        panic!()
    };
    let token = status.catalog.unwrap();
    let Response::Operation(Some(operation)) =
        export(&bridge, &token, Request::Paths { limit: U64(1) })?
    else {
        panic!()
    };
    events.recv_timeout(Duration::from_secs(5))?;
    let Response::Operation(Some(held)) = export(
        &bridge,
        &token,
        Request::Status {
            operation: Some(operation.id.clone()),
        },
    )?
    else {
        panic!()
    };
    assert!(held.write_hold);
    assert!(matches!(
        export(
            &bridge,
            &token,
            Request::Job {
                job: job.id.clone()
            }
        )?,
        Response::Job(_)
    ));
    let e = export(&bridge, &token, Request::Begin).unwrap_err();
    assert!(e.to_string().contains("hold"));
    export(
        &bridge,
        &token,
        Request::Cancel {
            job: None,
            operation: Some(operation.id.clone()),
        },
    )?;
    assert_eq!(terminal(&bridge, &token, &operation.id)?.phase, "canceled");
    released.store(true, Ordering::Release);
    let Response::Job(canceled) = export(
        &bridge,
        &token,
        Request::Cancel {
            job: Some(job.id.clone()),
            operation: None,
        },
    )?
    else {
        panic!("inactive cancel must retain Job envelope")
    };
    assert_eq!(canceled.state, "canceled");
    let Response::Operation(Some(last)) =
        export(&bridge, &token, Request::Status { operation: None })?
    else {
        panic!()
    };
    assert_eq!(last.kind, "cancel");
    assert_eq!(last.job.unwrap().id, job.id);
    // Close an actually held worker, without first waiting for cancellation.
    while events.try_recv().is_ok() {}
    released.store(false, Ordering::Release);
    export(&bridge, &token, Request::Paths { limit: U64(1) })?;
    events.recv_timeout(Duration::from_secs(5))?;
    call(&bridge, AppRequest::Close { catalog: token })?;
    bridge.shutdown();
    let c = Catalog::open(temp.path().join("catalog"))?;
    assert_eq!(c.photo_export_job(&job.id)?.state, "canceled");
    Ok(())
}

#[test]
fn destination_pages_keep_native_units_and_report_stale_targets() -> anyhow::Result<()> {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let (temp, mut c) = fixture()?;
    #[cfg(unix)]
    let key = {
        let original = NativePath::UnixBytes(b"/offline/native-\xff.png".to_vec());
        // A fresh offline original has this exact locator from first observation.
        // Existing locators can only change through reviewed relink, never here.
        c.db.execute("INSERT INTO assets(id,location,path_display,state,fingerprint,preview_hash,metadata) SELECT 'native',?1,'native fixture',state,fingerprint,preview_hash,metadata FROM assets WHERE id='a'", [crate::location_bytes(&original.to_path()?)])?;
        c.record_storage_path("native", &original)?;
        VariantKey::master("native")
    };
    #[cfg(not(unix))]
    let key = VariantKey::master("a");
    let ctx = worker::Context {
        control: Default::default(),
        cache: Default::default(),
        config: config(),
    };
    let targets = vec![
        TargetKey {
            key: key.clone(),
            expected_revision: I64(0),
        },
        TargetKey {
            key: key.clone(),
            expected_revision: I64(999),
        },
    ];
    let ResultValue::Destinations { token, total } = worker::destinations(
        &ctx,
        &c,
        &NativePath::from_path(temp.path()),
        targets,
        output().format,
        Naming {
            prefix: "out-".into(),
            suffix: "".into(),
            variant_suffix: false,
            sequence_start: None,
        },
    )?
    else {
        panic!()
    };
    assert_eq!(total, U64(2));
    let opts = worker::options(&ctx.config);
    let Response::Destinations { rows, next, .. } = read::execute(
        &c,
        Request::DestinationRows {
            token: token.clone(),
            after: U64(0),
            limit: U64(1),
        },
        &ctx.config.limits,
        &ctx.cache,
        &opts,
    )?
    else {
        panic!()
    };
    assert_eq!(next, Some(U64(1)));
    assert!(rows[0].error.is_none());
    #[cfg(unix)]
    assert!(
        matches!(&rows[0].destination,Some(NativePath::UnixBytes(bytes)) if bytes.ends_with(b"out-native-\xff.png"))
    );
    let Response::Destinations { rows, next, .. } = read::execute(
        &c,
        Request::DestinationRows {
            token: token.clone(),
            after: U64(1),
            limit: U64(1),
        },
        &ctx.config.limits,
        &ctx.cache,
        &opts,
    )?
    else {
        panic!()
    };
    assert!(next.is_none());
    assert!(rows[0].destination.is_none());
    assert!(rows[0].error.as_ref().unwrap().contains("changed"));
    read::execute(
        &c,
        Request::ResultRelease {
            token: token.clone(),
        },
        &ctx.config.limits,
        &ctx.cache,
        &opts,
    )?;
    assert!(
        read::execute(
            &c,
            Request::DestinationRows {
                token,
                after: U64(0),
                limit: U64(1)
            },
            &ctx.config.limits,
            &ctx.cache,
            &opts
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn final_job_read_never_holds_status_cancel_or_close_mutex() -> anyhow::Result<()> {
    let (temp, mut c) = fixture()?;
    let first = c.begin_photo_export()?;
    let second = c.begin_photo_export()?;
    drop(c);
    let (entered, events) = mpsc::channel();
    let mut config = config();
    config.import_checkpoint = Some(Arc::new(move |stage, cancel| {
        if stage == "export_final_job_read" {
            let _ = entered.send(());
            while !cancel.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(2));
            }
        }
    }));
    let bridge = Bridge::spawn(config)?;
    let AppResponse::Status(open) = call(
        &bridge,
        AppRequest::OpenExisting {
            path: NativePath::from_path(&temp.path().join("catalog")),
        },
    )?
    else {
        panic!()
    };
    let token = open.catalog.unwrap();
    let pending = bridge.submit(AppRequest::Export {
        catalog: token.clone(),
        request: Box::new(Request::Cancel {
            job: Some(first.id.clone()),
            operation: None,
        }),
    })?;
    events.recv_timeout(Duration::from_secs(5))?;
    let Response::Operation(Some(op)) =
        export(&bridge, &token, Request::Status { operation: None })?
    else {
        panic!()
    };
    assert_eq!(op.job.unwrap().id, first.id);
    export(
        &bridge,
        &token,
        Request::Cancel {
            job: Some(first.id),
            operation: Some(op.id.clone()),
        },
    )?;
    assert!(
        matches!(pending.receiver.recv_timeout(Duration::from_secs(5))?,Reply::Ok{value:AppResponse::Export(response)} if matches!(*response,Response::Job(ref job) if job.state=="canceled"))
    );
    assert_eq!(terminal(&bridge, &token, &op.id)?.phase, "complete"); // durable Cancel won its late cancellation
    let pending = bridge.submit(AppRequest::Export {
        catalog: token.clone(),
        request: Box::new(Request::Cancel {
            job: Some(second.id.clone()),
            operation: None,
        }),
    })?;
    events.recv_timeout(Duration::from_secs(5))?;
    // Close must signal the held worker before joining; no terminal wait first.
    call(&bridge, AppRequest::Close { catalog: token })?;
    assert!(matches!(
        pending.receiver.recv_timeout(Duration::from_secs(5))?,
        Reply::Ok { .. }
    ));
    bridge.shutdown();
    assert_eq!(
        Catalog::open(temp.path().join("catalog"))?
            .photo_export_job(&second.id)?
            .state,
        "canceled"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn close_keeps_export_nonterminal_and_permit_owned_until_native_reap() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let (temp, mut c) = fixture()?;
    let job = c.begin_photo_export()?;
    c.append_photo_export(
        &job.id,
        0,
        &core::ExportTarget {
            key: VariantKey::master("a"),
            expected_revision: 0,
            destination: temp.path().join("held.png"),
            overwrite: false,
            metadata: core::MetadataSelection::Omit,
        },
        &crate::image_export::OutputSpec {
            size: OutputSize::Original,
            format: output().format,
            profile: crate::image_export::OutputProfile::Srgb,
            alpha: AlphaPolicy::Preserve,
        },
        1024,
        1024,
    )?;
    c.seal_photo_export_job(&job.id, 1)?;
    drop(c);
    let executable = temp.path().join("held-worker");
    std::fs::write(&executable, b"#!/bin/sh\nexec /bin/sleep 60\n")?;
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))?;
    let (started, events) = mpsc::channel();
    let (reaping, at_reap) = mpsc::channel();
    let release = Arc::new(AtomicBool::new(false));
    let released = release.clone();
    let mut config = config();
    config.worker_executable = executable;
    config.import_checkpoint = Some(Arc::new(move |stage, cancel| {
        if let Some(pid) = stage.strip_prefix("export_native_started:") {
            let _ = started.send(pid.parse::<i32>().unwrap());
            while !cancel.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(2));
            }
        }
        if stage == "export_before_reap" {
            let _ = reaping.send(());
            let until = Instant::now() + Duration::from_secs(5);
            while !released.load(Ordering::Acquire) && Instant::now() < until {
                thread::sleep(Duration::from_millis(2));
            }
        }
    }));
    let bridge = Bridge::spawn(config)?;
    let AppResponse::Status(open) = call(
        &bridge,
        AppRequest::OpenExisting {
            path: NativePath::from_path(&temp.path().join("catalog")),
        },
    )?
    else {
        panic!()
    };
    let token = open.catalog.unwrap();
    let Response::Operation(Some(op)) = export(
        &bridge,
        &token,
        Request::Recover {
            directories: U64(32),
            limits: None,
        },
    )?
    else {
        panic!()
    };
    assert_eq!(terminal(&bridge, &token, &op.id)?.phase, "complete");
    let Response::Operation(Some(op)) = export(
        &bridge,
        &token,
        Request::Run {
            job: job.id.clone(),
            limits: None,
            max_items: U64(1),
            max_seconds: U64(30),
        },
    )?
    else {
        panic!()
    };
    let pid = events.recv_timeout(Duration::from_secs(5))?;
    assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
    let closing = bridge.submit(AppRequest::Close {
        catalog: token.clone(),
    })?;
    at_reap.recv_timeout(Duration::from_secs(5))?;
    let Response::Operation(Some(status)) = export(
        &bridge,
        &token,
        Request::Status {
            operation: Some(op.id),
        },
    )?
    else {
        panic!()
    };
    assert_eq!(status.stage, "draining");
    assert!(!["complete", "failed", "canceled", "paused"].contains(&status.phase.as_str()));
    assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
    assert!(bridge.0.shared.exports.lock().unwrap().active);
    release.store(true, Ordering::Release);
    assert!(matches!(
        closing.receiver.recv_timeout(Duration::from_secs(5))?,
        Reply::Ok { .. }
    ));
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    bridge.shutdown();
    let c = Catalog::open(temp.path().join("catalog"))?;
    assert_eq!(c.photo_export_job(&job.id)?.state, "canceled");
    assert_eq!(c.photo_export_items(&job.id, 0, 1)?[0].state, "rendering"); // explicit recovery owns unfinished evidence
    assert!(!temp.path().join("held.png").exists());
    Ok(())
}

#[cfg(unix)]
#[test]
fn rendering_allows_sibling_edit_and_foreground_preview_reaps_export_first() -> anyhow::Result<()> {
    use crate::application::{PreviewTier, Response as AppResponse};
    use std::os::unix::fs::PermissionsExt;
    let (temp, mut c) = fixture()?;
    let other = c
        .create_edit_variant(&VariantKey::master("a"), 0, "Other")?
        .key;
    let revision = c.edit_variant(&other)?.revision;
    let job = c.begin_photo_export()?;
    c.append_photo_export(
        &job.id,
        0,
        &core::ExportTarget {
            key: VariantKey::master("a"),
            expected_revision: 0,
            destination: temp.path().join("yielded.png"),
            overwrite: false,
            metadata: core::MetadataSelection::Omit,
        },
        &crate::image_export::OutputSpec {
            size: OutputSize::Original,
            format: output().format,
            profile: crate::image_export::OutputProfile::Srgb,
            alpha: AlphaPolicy::Preserve,
        },
        1024,
        1024,
    )?;
    c.seal_photo_export_job(&job.id, 1)?;
    drop(c);
    let executable = temp.path().join("held-worker");
    std::fs::write(&executable, b"#!/bin/sh\nexec /bin/sleep 60\n")?;
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))?;
    let (started, events) = mpsc::channel();
    let proceed = Arc::new(AtomicBool::new(false));
    let advance = proceed.clone();
    let mut config = config();
    config.worker_executable = executable;
    config.import_checkpoint = Some(Arc::new(move |stage, cancel| {
        if let Some(pid) = stage.strip_prefix("export_native_started:") {
            let _ = started.send(pid.parse::<i32>().unwrap());
            while !advance.load(Ordering::Acquire) && !cancel.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(2));
            }
        }
    }));
    let bridge = Bridge::spawn(config)?;
    let AppResponse::Status(open) = call(
        &bridge,
        AppRequest::OpenExisting {
            path: NativePath::from_path(&temp.path().join("catalog")),
        },
    )?
    else {
        panic!()
    };
    let token = open.catalog.unwrap();
    let Response::Operation(Some(op)) = export(
        &bridge,
        &token,
        Request::Recover {
            directories: U64(32),
            limits: None,
        },
    )?
    else {
        panic!()
    };
    assert_eq!(terminal(&bridge, &token, &op.id)?.phase, "complete");
    let Response::Operation(Some(op)) = export(
        &bridge,
        &token,
        Request::Run {
            job: job.id.clone(),
            limits: None,
            max_items: U64(1),
            max_seconds: U64(30),
        },
    )?
    else {
        panic!()
    };
    let pid = events.recv_timeout(Duration::from_secs(5))?;
    let saved = call(
        &bridge,
        AppRequest::SaveRecipe {
            catalog: token.clone(),
            key: other.clone(),
            expected_revision: I64(revision),
            recipe: crate::edit::Recipe::V1(crate::edit::RecipeV1 {
                exposure_ev: 1.0,
                ..Default::default()
            }),
        },
    )?;
    assert!(matches!(saved, AppResponse::Variant(_)));
    assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
    let AppResponse::Preview(preview) = call(
        &bridge,
        AppRequest::Preview {
            catalog: token.clone(),
            key: other,
            tier: PreviewTier::Thumbnail,
            interactive: false,
            viewport: "foreground".into(),
            generation: U64(1),
            foreground: true,
        },
    )?
    else {
        panic!()
    };
    proceed.store(true, Ordering::Release);
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let AppResponse::Status(status) = call(&bridge, AppRequest::Status)? else {
            panic!()
        };
        if status.active_previews > 0 {
            break;
        }
        anyhow::ensure!(
            Instant::now() < until,
            "foreground preview was not admitted"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    let Response::Operation(Some(waiting)) = export(
        &bridge,
        &token,
        Request::Status {
            operation: Some(op.id.clone()),
        },
    )?
    else {
        panic!()
    };
    assert!(!waiting.write_hold);
    export(
        &bridge,
        &token,
        Request::Cancel {
            job: Some(job.id.clone()),
            operation: Some(op.id.clone()),
        },
    )?;
    assert_eq!(terminal(&bridge, &token, &op.id)?.phase, "canceled");
    call(
        &bridge,
        AppRequest::CancelPreview {
            catalog: token.clone(),
            ticket: preview.ticket,
        },
    )?;
    call(&bridge, AppRequest::Close { catalog: token })?;
    bridge.shutdown();
    let c = Catalog::open(temp.path().join("catalog"))?;
    assert_eq!(c.photo_export_job(&job.id)?.state, "canceled");
    assert!(!temp.path().join("yielded.png").exists());
    Ok(())
}

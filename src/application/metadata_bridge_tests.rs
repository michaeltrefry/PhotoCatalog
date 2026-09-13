use super::*;

#[test]
fn metadata_bridge_preserves_selected_variant_revision_and_session() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("catalog");
    let mut catalog = Catalog::open(&root)?;
    catalog.db.execute(
        "INSERT INTO assets(id,location,path_display,state) VALUES('original',X'2F66697874757265','/fixture','pending')",
        [],
    )?;
    crate::organization::refresh(&catalog.db, "original")?;
    let master = VariantKey::master("original");
    let copy = catalog
        .create_edit_variant(&master, 0, "Selected copy")?
        .key;
    drop(catalog);
    let bridge = Bridge::spawn(Config {
        worker_executable: std::env::current_exe()?,
        cache_root: None,
        original_roots: vec![],
        preview_policy: preview::PreviewPolicy::default(),
        preview_limits: preview::ServiceLimits::default(),
        limits: Limits::default(),
        import_checkpoint: None,
    })?;
    let call = |request| -> Result<Response> {
        match bridge
            .submit(request)?
            .receiver
            .recv_timeout(Duration::from_secs(5))?
        {
            Reply::Ok { value } => Ok(value),
            Reply::Error { error } => Err(error.into()),
        }
    };
    let Response::Status(opened) = call(Request::OpenExisting {
        path: NativePath::from_path(&root),
    })?
    else {
        anyhow::bail!("open response")
    };
    let token = opened.catalog.context("catalog session")?;
    let request = |token: &str, request| Request::Metadata {
        catalog: token.into(),
        request: Box::new(request),
    };
    let Response::Metadata(value) = call(request(
        &token,
        metadata::Request::Identity { key: copy.clone() },
    ))?
    else {
        anyhow::bail!("metadata response")
    };
    let metadata::Response::Identity(identity) = *value else {
        anyhow::bail!("identity response")
    };
    assert_eq!(identity.key, copy);
    let Response::Metadata(value) = call(request(
        &token,
        metadata::Request::Fields {
            identity: identity.clone(),
            after: None,
            limit: 20,
        },
    ))?
    else {
        anyhow::bail!("fields response")
    };
    assert!(matches!(*value, metadata::Response::Fields(_)));
    call(Request::Cull {
        catalog: token.clone(),
        key: copy.clone(),
        expected_revision: identity.metadata_revision,
        operation: CullOperation::Rating(4),
    })?;
    assert!(
        call(request(
            &token,
            metadata::Request::Fields {
                identity,
                after: None,
                limit: 20
            }
        ))
        .is_err(),
        "stale selected metadata accepted"
    );
    let Response::Metadata(value) =
        call(request(&token, metadata::Request::Identity { key: master }))?
    else {
        anyhow::bail!("master response")
    };
    let metadata::Response::Identity(master_identity) = *value else {
        anyhow::bail!("master identity")
    };
    assert_eq!(
        master_identity.metadata_revision.0, 0,
        "copy culling changed master"
    );
    call(Request::Close {
        catalog: token.clone(),
    })?;
    call(Request::OpenExisting {
        path: NativePath::from_path(&root),
    })?;
    let reply = bridge
        .submit(request(&token, metadata::Request::Identity { key: copy }))?
        .receiver
        .recv_timeout(Duration::from_secs(5))?;
    assert!(matches!(
        reply,
        Reply::Error {
            error: BridgeError {
                code: ErrorCode::StaleSession,
                ..
            }
        }
    ));
    bridge.shutdown();
    Ok(())
}

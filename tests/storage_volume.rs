//! Synthetic mount tables and disposable local files; never detach user storage.
#[path = "../src/storage_volume.rs"]
#[allow(dead_code)]
mod storage_volume;
use std::path::{Path, PathBuf};
use storage_volume::*;

fn identity() -> PersistentVolumeId {
    PersistentVolumeId::new(
        IdentityScheme::MacVolumeUuid,
        "A88E634C-47BA-440F-831A-6E631274439B",
    )
    .unwrap()
}
fn mount(root: &Path, id: Option<PersistentVolumeId>) -> MountedVolume {
    MountedVolume {
        mount_path: NativePath::from_path(root),
        volume_subpath: NativePath::from_path(Path::new("")),
        persistent_identity: id,
        filesystem: "fixture".into(),
        device_number: None,
        issues: vec![],
    }
}
fn native_root() -> PathBuf {
    #[cfg(unix)]
    {
        PathBuf::from("/fixture")
    }
    #[cfg(windows)]
    {
        PathBuf::from(r"C:\fixture")
    }
}
#[test]
fn logical_identity_is_independent_and_persistable() {
    let a = LogicalVolume::new(Some(identity()));
    let b = LogicalVolume::new(Some(identity()));
    assert_ne!(a.id, b.id);
    assert_eq!(a.persistent_identity, b.persistent_identity);
    assert_eq!(
        serde_json::from_slice::<LogicalVolume>(&serde_json::to_vec(&a).unwrap()).unwrap(),
        a
    );
    assert!(uuid::Uuid::parse_str(&a.id).is_ok());
    assert_eq!(identity().value, "a88e634c-47ba-440f-831a-6e631274439b");
    assert!(PersistentVolumeId::new(IdentityScheme::MacVolumeUuid, "volume name").is_err());
    assert!(PersistentVolumeId::new(IdentityScheme::LinuxFilesystemUuid, "../fake").is_err());
}
#[test]
fn duplicates_and_partial_snapshots_never_guess() {
    let volume = LogicalVolume::new(Some(identity()));
    let root = native_root();
    let a = mount(&root, Some(identity()));
    let mut snapshot = MountSnapshot {
        mounts: vec![a.clone()],
        complete: true,
        issues: vec![],
    };
    assert_eq!(match_mounts(&volume, &snapshot), MountMatch::Unique(a));
    snapshot.complete = false;
    assert_eq!(match_mounts(&volume, &snapshot), MountMatch::Indeterminate);
    snapshot
        .mounts
        .push(mount(&root.join("alias"), Some(identity())));
    assert!(
        matches!(match_mounts(&volume, &snapshot), MountMatch::Ambiguous(ref v) if v.len() == 2)
    );
    snapshot.mounts.clear();
    assert_eq!(match_mounts(&volume, &snapshot), MountMatch::Indeterminate);
    snapshot.complete = true;
    assert_eq!(match_mounts(&volume, &snapshot), MountMatch::Offline);
    assert_eq!(
        match_mounts(&LogicalVolume::new(None), &snapshot),
        MountMatch::IdentityUnavailable
    );
    snapshot.mounts.push(mount(&root, None));
    // Reusing an old mount path without its identity never matches.
    assert_eq!(match_mounts(&volume, &snapshot), MountMatch::Offline);
}
#[test]
fn relative_mapping_respects_bind_roots_and_rejects_escape() {
    let mut volume = mount(&native_root(), Some(identity()));
    volume.volume_subpath = NativePath::from_path(Path::new("photos"));
    let relative = NativePath::from_path(Path::new("photos/2020/image.cr2"));
    assert_eq!(
        candidate_path(&volume, &relative).unwrap(),
        native_root().join("2020/image.cr2")
    );
    for bad in ["other/image.cr2", "photos/../outside", "../outside"] {
        assert!(candidate_path(&volume, &NativePath::from_path(Path::new(bad))).is_err());
    }
    assert!(candidate_path(&volume, &NativePath::from_path(&native_root())).is_err());
}
#[test]
fn native_paths_retain_non_unicode_and_reject_foreign_interpretation() {
    #[cfg(unix)]
    let (path, foreign) = {
        use std::os::unix::ffi::OsStringExt;
        (
            PathBuf::from(std::ffi::OsString::from_vec(b"/photos/\xff.CR2".to_vec())),
            NativePath::WindowsWide(vec![0xd800]),
        )
    };
    #[cfg(windows)]
    let (path, foreign) = {
        use std::os::windows::ffi::OsStringExt;
        (
            PathBuf::from(std::ffi::OsString::from_wide(&[
                67, 58, 92, 0xd800, 46, 106, 112, 103,
            ])),
            NativePath::UnixBytes(vec![255]),
        )
    };
    let encoded = NativePath::from_path(&path);
    assert_eq!(encoded.to_path().unwrap(), path);
    assert_eq!(
        serde_json::from_slice::<NativePath>(&serde_json::to_vec(&encoded).unwrap()).unwrap(),
        encoded
    );
    assert!(foreign.to_path().is_err());
    assert!(NativePath::UnixBytes(vec![0]).to_path().is_err());
    assert!(NativePath::WindowsWide(vec![0]).to_path().is_err());
}
#[test]
fn linux_mountinfo_retains_escaped_bytes_and_internal_subvolume_root() {
    let table = b"31 20 8:2 / / rw,relatime - ext4 /dev/sda2 rw\n32 31 8:3 /library /media/My\\040Photos rw shared:12 - ext4 /dev/sdb1 rw\n33 31 8:4 / /media/\xff\\134disk rw - ext4 /dev/sdc1 rw\n";
    let parsed = parse_linux_mountinfo(table);
    assert!(parsed.complete);
    assert_eq!(parsed.mounts.len(), 3);
    assert_eq!(
        parsed.mounts[1].mount_path,
        NativePath::UnixBytes(b"/media/My Photos".to_vec())
    );
    assert_eq!(
        parsed.mounts[1].volume_subpath,
        NativePath::UnixBytes(b"library".to_vec())
    );
    assert_eq!(
        parsed.mounts[2].mount_path,
        NativePath::UnixBytes(b"/media/\xff\\disk".to_vec())
    );
    assert!(
        parsed
            .mounts
            .iter()
            .all(|m| m.persistent_identity.is_none())
    );
    assert_eq!(
        parsed.mounts[1].device_number,
        Some(DeviceNumber { major: 8, minor: 3 })
    );
}
#[test]
fn malformed_linux_records_are_explicit_and_keep_valid_prefix() {
    for bad in [
        "broken",
        "2 1 8:2 / /bad\\777 rw - ext4 dev rw",
        "2 1 8:2 /../a /mnt rw - ext4 dev rw",
        "2 1 999999999999:2 / /mnt rw - ext4 dev rw",
        "bogus 1 8:2 / /mnt rw - ext4 dev rw",
    ] {
        let bytes = format!("1 0 8:1 / / rw - ext4 /dev/a rw\n{bad}\n");
        let parsed = parse_linux_mountinfo(bytes.as_bytes());
        assert!(!parsed.complete, "{bad}");
        assert_eq!(parsed.mounts.len(), 1);
        assert!(parsed.issues.iter().any(|i| i.kind == IssueKind::Malformed));
    }
    assert!(!parse_linux_mountinfo(b"").complete);
    let large = parse_linux_mountinfo(&vec![b'x'; 4 * 1024 * 1024 + 1]);
    assert!(!large.complete);
    assert_eq!(large.issues[0].kind, IssueKind::ResourceLimit);
    let many = parse_linux_mountinfo("1 0 8:1 / / rw - ext4 dev rw\n".repeat(4097).as_bytes());
    assert!(!many.complete);
    assert_eq!(many.mounts.len(), 4096);
}
#[test]
fn windows_guid_parser_preserves_unpaired_surrogates_and_rejects_traversal() {
    let root = r"\\?\Volume{A88E634C-47BA-440F-831A-6E631274439B}\";
    let mut units: Vec<_> = root.encode_utf16().collect();
    units.extend([0xd800, 92, 97]);
    let (id, suffix) = parse_windows_volume_guid_path(&units).unwrap();
    assert_eq!(id.scheme, IdentityScheme::WindowsVolumeGuid);
    assert_eq!(id.value, identity().value);
    assert_eq!(suffix, NativePath::WindowsWide(vec![0xd800, 92, 97]));
    for bad in [r"..\outside", r"\root", r"folder\..\outside", r"x:stream"] {
        assert!(
            parse_windows_volume_guid_path(
                &format!("{root}{bad}").encode_utf16().collect::<Vec<_>>()
            )
            .is_err()
        );
    }
    assert!(
        parse_windows_volume_guid_path(&r"\\server\share\photo".encode_utf16().collect::<Vec<_>>())
            .is_err()
    );
    assert!(parse_windows_volume_guid_path(&[0]).is_err());
}
#[test]
fn existing_and_missing_paths_have_separate_evidence_without_source_writes() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let source = root.join("photo.jpg");
    std::fs::write(&source, b"synthetic content").unwrap();
    let before = std::fs::read(&source).unwrap();
    let existing = locate(&source);
    assert_eq!(existing.state, LocationState::Available, "{existing:?}");
    assert!(existing.volume.is_some());
    assert!(existing.existing_ancestor.is_none());
    let missing = locate(&root.join("offline/photos/not-there.cr2"));
    assert_eq!(missing.state, LocationState::MissingPath, "{missing:?}");
    assert!(missing.volume.is_none());
    assert!(missing.relative_in_volume.is_none());
    let ancestor = missing.existing_ancestor.unwrap();
    assert_eq!(ancestor.path.to_path().unwrap(), root);
    assert_eq!(
        ancestor.unresolved_suffix.to_path().unwrap(),
        Path::new("offline/photos/not-there.cr2")
    );
    assert_eq!(std::fs::read(&source).unwrap(), before);
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
    assert_ne!(
        locate(Path::new("relative/missing")).state,
        LocationState::Available
    );
}
#[cfg(unix)]
#[test]
fn symlink_alias_maps_actual_volume_and_special_nodes_are_not_opened() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    std::fs::create_dir(root.join("actual")).unwrap();
    std::os::unix::fs::symlink(root.join("actual"), root.join("alias")).unwrap();
    let actual = locate(&root.join("actual"));
    let alias = locate(&root.join("alias"));
    assert_eq!(alias.state, LocationState::Available);
    assert_eq!(alias.canonical_path, actual.canonical_path);
    assert_eq!(alias.relative_in_volume, actual.relative_in_volume);
    use std::os::unix::net::UnixListener;
    let _listener = UnixListener::bind(root.join("socket")).unwrap();
    assert_eq!(
        locate(&root.join("socket")).state,
        LocationState::Unsupported
    );
}
/// Read-only host fixture hook: the coordinator supplies a disposable mounted
/// volume path. No volume creation/detach commands are embedded in test code.
#[test]
#[ignore = "requires an explicitly prepared disposable volume and released hardware lane"]
fn inspect_explicit_volume_fixture() {
    let root =
        std::env::var_os("PHOTOCATALOG_VOLUME_FIXTURE").expect("explicit disposable fixture path");
    let observed = locate(Path::new(&root));
    assert_eq!(observed.state, LocationState::Available, "{observed:?}");
    assert!(observed.relative_in_volume.is_some(), "{observed:?}");
    let id = observed
        .volume
        .as_ref()
        .unwrap()
        .persistent_identity
        .as_ref()
        .expect("fixture must provide a persistent UUID");
    if let Ok(expected) = std::env::var("PHOTOCATALOG_EXPECT_VOLUME_ID") {
        assert_eq!(id.value, expected);
    }
    println!("{}", serde_json::to_string(&observed).unwrap());
    let snapshot = mounted_volumes().unwrap();
    assert!(
        matches!(
            match_mounts(&LogicalVolume::new(Some(id.clone())), &snapshot),
            MountMatch::Unique(_) | MountMatch::Ambiguous(_)
        ),
        "{snapshot:?}"
    );
}

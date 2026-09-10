use anyhow::Result;
use photocatalog::metadata_export::*;
use std::fs;

#[test]
fn held_proof_rejects_restored_mtime_replacement_and_bounded_scan_mutation() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("ordinary-file");
    fs::write(&path, b"original")?;
    assert!(VerifiedFile::read(&path, 7).is_err());
    let proof = VerifiedFile::read(&path, 8)?;
    let mtime = fs::metadata(&path)?.modified()?;
    #[cfg(not(windows))]
    {
        fs::write(&path, b"modified")?;
        fs::File::options()
            .write(true)
            .open(&path)?
            .set_times(fs::FileTimes::new().set_modified(mtime))?;
        assert!(proof.recheck().is_err());
    }
    #[cfg(windows)]
    {
        assert_eq!(
            fs::write(&path, b"modified").unwrap_err().raw_os_error(),
            Some(32)
        );
        assert_eq!(fs::read(&path)?, b"original");
        proof.recheck()?;
    }
    drop(proof);
    // Lease release allows a normal same-size edit; a new full proof sees it.
    fs::write(&path, b"modified")?;
    let proof = VerifiedFile::read(&path, 8)?;
    fs::rename(&path, root.path().join("held-old-object"))?;
    fs::write(&path, b"modified")?;
    fs::File::options()
        .write(true)
        .open(&path)?
        .set_times(fs::FileTimes::new().set_modified(mtime))?;
    assert!(proof.recheck().is_err());
    drop(proof);
    #[cfg(not(windows))]
    let mut changed = false;
    #[cfg(windows)]
    let changed = false;
    assert!(
        VerifiedFile::read_with_checkpoint(&path, 8, &mut |_| {
            if !changed {
                #[cfg(windows)]
                {
                    let err = fs::write(&path, b"external").unwrap_err();
                    assert_eq!(err.raw_os_error(), Some(32));
                    return Err(err);
                }
                #[cfg(not(windows))]
                {
                    fs::write(&path, b"external")?;
                    changed = true;
                }
            }
            Ok(())
        })
        .is_err()
    );
    #[cfg(not(windows))]
    assert_eq!(fs::read(path)?, b"external");
    #[cfg(windows)]
    assert_eq!(fs::read(path)?, b"modified");
    Ok(())
}

#[test]
fn phased_lease_and_destination_proof_preserve_a_concurrent_replacement() -> Result<()> {
    let root = tempfile::tempdir()?;
    let destination = root.path().join("output.jpg");
    let input = root.path().join("encoded");
    fs::write(&destination, b"old destination")?;
    fs::write(&input, b"encoded bytes")?;
    let snapshot = snapshot_photo_destination(&destination, 1024)?;
    let authority = blake3::hash(b"frozen-job-plan").to_hex().to_string();
    let seal = seal_photo_export(&snapshot, &input, 1024, &authority, |_| Ok(()))?;
    let mut publication = PhotoPublication::prepare(&seal)?;
    assert!(PhotoPublication::prepare(&seal).is_err());
    assert!(publish_photo_export(&seal).is_err());
    fs::rename(&destination, root.path().join("old-retained"))?;
    fs::write(&destination, b"external replacement")?;
    assert!(publication.capture().is_err());
    assert_eq!(fs::read(&destination)?, b"external replacement");
    assert!(!seal.recovery_directory().join("original").exists());
    assert_eq!(fs::read(input)?, b"encoded bytes");
    Ok(())
}

#[cfg(windows)]
#[test]
fn windows_proof_refuses_existing_writer_and_writable_mapping() -> Result<()> {
    use std::{ffi::c_void, os::windows::io::AsRawHandle};
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateFileMappingW(
            file: *mut c_void,
            attributes: *mut c_void,
            protect: u32,
            high: u32,
            low: u32,
            name: *const u16,
        ) -> *mut c_void;
        fn MapViewOfFile(
            mapping: *mut c_void,
            access: u32,
            high: u32,
            low: u32,
            bytes: usize,
        ) -> *mut c_void;
        fn UnmapViewOfFile(view: *const c_void) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }
    struct Mapping {
        handle: *mut c_void,
        view: *mut c_void,
    }
    impl Drop for Mapping {
        fn drop(&mut self) {
            unsafe {
                UnmapViewOfFile(self.view);
                CloseHandle(self.handle);
            }
        }
    }
    let root = tempfile::tempdir()?;
    let path = root.path().join("source");
    fs::write(&path, b"original")?;
    let writer = fs::File::options().read(true).write(true).open(&path)?;
    let error = VerifiedFile::read(&path, 8)
        .err()
        .expect("existing writer must block proof");
    assert_eq!(
        error
            .downcast_ref::<std::io::Error>()
            .unwrap()
            .raw_os_error(),
        Some(32)
    );
    let handle = unsafe {
        CreateFileMappingW(
            writer.as_raw_handle(),
            std::ptr::null_mut(),
            4,
            0,
            0,
            std::ptr::null(),
        )
    }; // PAGE_READWRITE
    assert!(!handle.is_null(), "{}", std::io::Error::last_os_error());
    let view = unsafe { MapViewOfFile(handle, 2, 0, 0, 8) }; // FILE_MAP_WRITE
    if view.is_null() {
        let error = std::io::Error::last_os_error();
        unsafe {
            CloseHandle(handle);
        }
        return Err(error.into());
    }
    let mapping = Mapping { handle, view };
    drop(writer); // A live writable mapping must still exclude our proof.
    let error = VerifiedFile::read(&path, 8)
        .err()
        .expect("writable mapping must block proof");
    assert_eq!(
        error
            .downcast_ref::<std::io::Error>()
            .unwrap()
            .raw_os_error(),
        Some(32)
    );
    unsafe {
        std::ptr::copy_nonoverlapping(b"modified".as_ptr(), mapping.view.cast::<u8>(), 8);
    }
    drop(mapping);
    assert_eq!(fs::read(&path)?, b"modified");
    let proof = VerifiedFile::read(&path, 8)?;
    proof.recheck()?;
    assert_eq!(
        proof.revision().digest,
        blake3::hash(b"modified").to_hex().to_string()
    );
    Ok(())
}

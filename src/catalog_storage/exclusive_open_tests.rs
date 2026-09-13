use anyhow::{Context, Result, ensure};
use rusqlite::ffi;
use std::{ffi::CString, path::Path};

// Exercise the public VFS boundary used by SQLite's temporary-file opener.
// The existing hardlink makes exclusive creation fail deterministically; no
// syscall hooks, random-name race, or user database is involved.
struct VfsFile {
    file: *mut ffi::sqlite3_file,
    // The VFS may retain this filename until xClose.
    name: Filename,
}
struct Filename(ffi::sqlite3_filename);
impl Drop for Filename {
    fn drop(&mut self) {
        unsafe { ffi::sqlite3_free_filename(self.0) };
    }
}
impl VfsFile {
    fn open(path: &Path) -> Result<(Self, i32, i32)> {
        ensure!(unsafe { ffi::sqlite3_initialize() } == ffi::SQLITE_OK);
        let vfs = unsafe { ffi::sqlite3_vfs_find(std::ptr::null()) };
        ensure!(!vfs.is_null(), "default SQLite VFS is unavailable");
        let size = unsafe { (*vfs).szOsFile };
        ensure!(size >= std::mem::size_of::<ffi::sqlite3_file>() as i32);
        // VFS URI accessors require SQLite's allocated filename, including its
        // hidden prefix. A CString with trailing terminators is insufficient.
        let name = CString::new(
            path.to_str()
                .context("synthetic VFS test path must be UTF-8")?,
        )?;
        let name = Filename(unsafe {
            ffi::sqlite3_create_filename(
                name.as_ptr(),
                c"".as_ptr(),
                c"".as_ptr(),
                0,
                std::ptr::null_mut(),
            )
        });
        ensure!(!name.0.is_null(), "VFS filename allocation failed");
        let pointer = unsafe { ffi::sqlite3_malloc64(size as u64) };
        ensure!(!pointer.is_null(), "VFS file allocation failed");
        unsafe { std::ptr::write_bytes(pointer.cast::<u8>(), 0, size as usize) };
        let file = Self {
            file: pointer.cast(),
            name,
        };
        let flags = ffi::SQLITE_OPEN_READWRITE
            | ffi::SQLITE_OPEN_CREATE
            | ffi::SQLITE_OPEN_EXCLUSIVE
            | ffi::SQLITE_OPEN_DELETEONCLOSE
            | ffi::SQLITE_OPEN_TEMP_DB;
        let mut output = 0;
        let open = unsafe { (*vfs).xOpen }.context("default VFS has no opener")?;
        let result = unsafe { open(vfs, file.name.0, file.file, flags, &mut output) };
        Ok((file, result, output))
    }

    fn close(&mut self) -> Result<()> {
        let methods = unsafe { (*self.file).pMethods };
        if !methods.is_null() {
            let close = unsafe { (*methods).xClose }.context("opened VFS file has no close")?;
            let code = unsafe { close(self.file) };
            unsafe { (*self.file).pMethods = std::ptr::null() };
            ensure!(code == ffi::SQLITE_OK, "VFS close failed: {code}");
        }
        Ok(())
    }
}
impl Drop for VfsFile {
    fn drop(&mut self) {
        let _ = self.close();
        unsafe { ffi::sqlite3_free(self.file.cast()) };
    }
}

#[test]
fn exclusive_temporary_open_preserves_existing_hardlink() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let original = temp.path().join("original.bin");
    let collision = temp.path().join("temporary-name");
    let contents = b"an existing object must never become a temporary database";
    std::fs::write(&original, contents)?;
    std::fs::hard_link(&original, &collision)?;
    let (mut file, result, output) = VfsFile::open(&collision)?;
    file.close()?;
    assert_eq!(
        result & 0xff,
        ffi::SQLITE_CANTOPEN,
        "exclusive open admitted an existing file; output flags={output:#x}, alias still present={}",
        collision.exists()
    );
    assert_eq!(std::fs::read(&original)?, contents);
    assert_eq!(std::fs::read(&collision)?, contents);
    Ok(())
}

#[test]
fn exclusive_temporary_open_still_creates_and_retires_new_file() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("new-temporary-name");
    let (mut file, result, output) = VfsFile::open(&path)?;
    assert_eq!(result, ffi::SQLITE_OK);
    assert_ne!(output & ffi::SQLITE_OPEN_READWRITE, 0);
    assert_eq!(output & ffi::SQLITE_OPEN_READONLY, 0);
    file.close()?;
    assert!(!path.exists(), "temporary file survived normal close");
    Ok(())
}

//! Original file handles and default SQLite cooperative byte locks. No SQLite
//! connection is opened here. The capture CLI owns an otherwise isolated process.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    time::UNIX_EPOCH,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revision {
    pub object: String,
    pub bytes: u64,
    pub modified_ns: Option<u128>,
    pub changed: String,
}
fn revision(file: &File) -> Result<Revision> {
    let m = file.metadata()?;
    ensure!(
        m.is_file() && !m.file_type().is_symlink(),
        "source handle is not a regular file"
    );
    #[cfg(unix)]
    let (object, changed) = {
        use std::os::unix::fs::MetadataExt;
        (
            format!("{}:{}", m.dev(), m.ino()),
            format!("{}:{}", m.ctime(), m.ctime_nsec()),
        )
    };
    #[cfg(windows)]
    let (object, changed) = windows::identity(file)?;
    Ok(Revision {
        object,
        changed,
        bytes: m.len(),
        modified_ns: m
            .modified()
            .ok()
            .and_then(|v| v.duration_since(UNIX_EPOCH).ok())
            .map(|v| v.as_nanos()),
    })
}
pub(super) fn reject_links(path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        if matches!(component, Component::Prefix(_) | Component::RootDir) {
            continue;
        }
        let meta = fs::symlink_metadata(&current)?;
        ensure!(
            !meta.file_type().is_symlink(),
            "source path contains a link: {}",
            current.display()
        );
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            ensure!(
                meta.file_attributes() & 0x400 == 0,
                "source path contains a reparse point"
            );
        }
    }
    Ok(())
}
pub(super) struct Source {
    pub path: PathBuf,
    pub file: File,
    pub before: Revision,
    locks: Vec<(u64, u64)>,
}
impl Source {
    pub fn open(path: &Path, maximum: u64) -> Result<Self> {
        reject_links(path)?;
        let metadata = fs::symlink_metadata(path)?;
        ensure!(metadata.is_file(), "source is not a regular file");
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(0x0020_0000);
        }
        let file = options.open(path)?;
        let before = revision(&file)?;
        ensure!(
            before.bytes <= maximum,
            "source file exceeds declared byte limit"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            ensure!(
                before.object == format!("{}:{}", metadata.dev(), metadata.ino()),
                "source replaced during open"
            );
        }
        Ok(Self {
            path: path.into(),
            file,
            before,
            locks: vec![],
        })
    }
    pub fn lock(&mut self, start: u64, length: u64) -> Result<()> {
        set_lock(&self.file, start, length, true)
            .context("active writer or unavailable cooperative source lock")?;
        self.locks.push((start, length));
        Ok(())
    }
    pub fn verify(&self) -> Result<()> {
        ensure!(
            revision(&self.file)? == self.before,
            "source handle revision changed"
        );
        reject_links(&self.path)?;
        let metadata = fs::symlink_metadata(&self.path)?;
        ensure!(metadata.is_file(), "source path became non-regular");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            ensure!(
                self.before.object == format!("{}:{}", metadata.dev(), metadata.ino()),
                "source path replaced"
            );
            ensure!(
                self.before.bytes == metadata.len()
                    && self.before.changed
                        == format!("{}:{}", metadata.ctime(), metadata.ctime_nsec()),
                "source path revision changed"
            );
        }
        #[cfg(windows)]
        {
            // Windows locks are handle scoped, so closing this additional identity
            // handle cannot release the capture handle's byte-range locks.
            let path_handle = Source::open(&self.path, self.before.bytes)?;
            ensure!(path_handle.before == self.before, "source path replaced");
        }
        Ok(())
    }
    pub fn copy_and_hash(&mut self, mut output: Option<&mut File>) -> Result<String> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut remaining = self.before.bytes;
        let mut hash = blake3::Hasher::new();
        let mut bytes = [0u8; 128 * 1024];
        while remaining > 0 {
            let count = remaining.min(bytes.len() as u64) as usize;
            self.file
                .read_exact(&mut bytes[..count])
                .context("source truncated")?;
            hash.update(&bytes[..count]);
            if let Some(file) = output.as_mut() {
                file.write_all(&bytes[..count])?;
            }
            remaining -= count as u64;
        }
        self.verify()?;
        Ok(hash.finalize().to_hex().to_string())
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        for &(start, length) in self.locks.iter().rev() {
            let _ = set_lock(&self.file, start, length, false);
        }
    }
}
#[cfg(unix)]
fn set_lock(file: &File, start: u64, length: u64, lock: bool) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let mut value: libc::flock = unsafe { std::mem::zeroed() };
    value.l_type = (if lock { libc::F_RDLCK } else { libc::F_UNLCK }) as _;
    value.l_whence = libc::SEEK_SET as _;
    value.l_start = start
        .try_into()
        .map_err(|_| std::io::Error::other("lock offset overflow"))?;
    value.l_len = length
        .try_into()
        .map_err(|_| std::io::Error::other("lock length overflow"))?;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &value) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
#[cfg(windows)]
fn set_lock(file: &File, start: u64, length: u64, lock: bool) -> std::io::Result<()> {
    windows::set_lock(file, start, length, lock)
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::{ffi::c_void, os::windows::io::AsRawHandle};
    #[repr(C)]
    struct Overlapped {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        event: *mut c_void,
    }
    #[repr(C)]
    struct FileId {
        volume: u64,
        id: [u8; 16],
    }
    #[repr(C)]
    struct Basic {
        creation: i64,
        access: i64,
        write: i64,
        change: i64,
        attributes: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LockFileEx(
            file: *mut c_void,
            flags: u32,
            reserved: u32,
            low: u32,
            high: u32,
            overlapped: *mut Overlapped,
        ) -> i32;
        fn UnlockFileEx(
            file: *mut c_void,
            reserved: u32,
            low: u32,
            high: u32,
            overlapped: *mut Overlapped,
        ) -> i32;
        fn GetFileInformationByHandleEx(
            file: *mut c_void,
            class: i32,
            data: *mut c_void,
            size: u32,
        ) -> i32;
    }
    pub fn identity(file: &File) -> Result<(String, String)> {
        let mut id: FileId = unsafe { std::mem::zeroed() };
        let mut basic: Basic = unsafe { std::mem::zeroed() };
        ensure!(
            unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    18,
                    (&mut id as *mut FileId).cast(),
                    std::mem::size_of::<FileId>() as u32,
                )
            } != 0,
            "Windows source identity unavailable"
        );
        ensure!(
            unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    0,
                    (&mut basic as *mut Basic).cast(),
                    std::mem::size_of::<Basic>() as u32,
                )
            } != 0,
            "Windows source change time unavailable"
        );
        ensure!(
            basic.attributes & 0x400 == 0,
            "source handle is a reparse point"
        );
        Ok((
            format!("{}:{}", id.volume, u128::from_ne_bytes(id.id)),
            basic.change.to_string(),
        ))
    }
    pub fn set_lock(file: &File, start: u64, length: u64, lock: bool) -> std::io::Result<()> {
        let length = if length == 0 {
            u64::MAX - start
        } else {
            length
        };
        let mut overlapped = Overlapped {
            internal: 0,
            internal_high: 0,
            offset: start as u32,
            offset_high: (start >> 32) as u32,
            event: std::ptr::null_mut(),
        };
        let ok = unsafe {
            if lock {
                LockFileEx(
                    file.as_raw_handle(),
                    1,
                    0,
                    length as u32,
                    (length >> 32) as u32,
                    &mut overlapped,
                )
            } else {
                UnlockFileEx(
                    file.as_raw_handle(),
                    0,
                    length as u32,
                    (length >> 32) as u32,
                    &mut overlapped,
                )
            }
        };
        if ok == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        process::{Child, Command, Stdio},
        thread,
        time::{Duration, Instant},
    };
    struct Worker(Child);
    impl Drop for Worker {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    #[test]
    #[ignore = "subprocess-only cooperative lock holder"]
    fn lock_worker() {
        let path =
            std::env::var_os("PHOTOCATALOG_LR_TEST_LOCK_PATH").expect("isolated worker path");
        let ready =
            std::env::var_os("PHOTOCATALOG_LR_TEST_READY").expect("isolated readiness path");
        let mut source = Source::open(Path::new(&path), 1024 * 1024).unwrap();
        source.lock(128, 1).unwrap();
        source.lock(120, 8).unwrap();
        fs::write(ready, b"locked").unwrap();
        let mut byte = [0];
        std::io::stdin().read_exact(&mut byte).unwrap();
    }
    fn fixture(root: &Path) {
        fs::create_dir(root).unwrap();
        let source = root.join("construction.sqlite3");
        let db = rusqlite::Connection::open(&source).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL;PRAGMA wal_autocheckpoint=0;CREATE TABLE evidence(v);INSERT INTO evidence VALUES(7);").unwrap();
        for (from, to) in [
            ("construction.sqlite3", "catalog.sqlite3"),
            ("construction.sqlite3-wal", "catalog.sqlite3-wal"),
            ("construction.sqlite3-shm", "catalog.sqlite3-shm"),
        ] {
            fs::copy(root.join(from), root.join(to)).unwrap();
        }
        drop(db);
        let mut shm = OpenOptions::new()
            .write(true)
            .open(root.join("catalog.sqlite3-shm"))
            .unwrap();
        shm.set_len(65536).unwrap();
        shm.seek(SeekFrom::Start(65532)).unwrap();
        shm.write_all(b"DMS!").unwrap();
    }
    #[test]
    fn deadman_lock_prevents_fresh_sqlite_opener_from_resetting_quiescent_shm() {
        let temp = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let control = temp.path().join("control");
        let guarded = temp.path().join("guarded");
        fixture(&control);
        fixture(&guarded);
        // Positive control: the real SQLite default VFS removes the extra SHM
        // region when no other process holds the deadman shared lock.
        let db = rusqlite::Connection::open(control.join("catalog.sqlite3")).unwrap();
        assert_eq!(
            db.query_row("SELECT v FROM evidence", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            7
        );
        assert!(
            fs::metadata(control.join("catalog.sqlite3-shm"))
                .unwrap()
                .len()
                < 65536
        );
        drop(db);
        let ready = temp.path().join("ready");
        let shm = guarded.join("catalog.sqlite3-shm");
        let before = fs::read(&shm).unwrap();
        let mut child = Worker(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "lightroom::source::tests::lock_worker",
                    "--nocapture",
                ])
                .env("PHOTOCATALOG_LR_TEST_LOCK_PATH", &shm)
                .env("PHOTOCATALOG_LR_TEST_READY", &ready)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() {
            assert!(Instant::now() < deadline, "lock child did not become ready");
            assert!(child.0.try_wait().unwrap().is_none(), "lock child exited");
            thread::sleep(Duration::from_millis(10));
        }
        let db = rusqlite::Connection::open(guarded.join("catalog.sqlite3")).unwrap();
        let value = db.query_row("SELECT v FROM evidence", [], |r| r.get::<_, i64>(0));
        // A compatible existing read mark may permit the read. BUSY is also a
        // valid cooperative outcome, but neither outcome may reset captured SHM.
        if let Ok(value) = value {
            assert_eq!(value, 7);
        }
        assert_eq!(fs::read(&shm).unwrap(), before);
        drop(db);
        assert_eq!(fs::read(&shm).unwrap(), before);
        child.0.stdin.take().unwrap().write_all(&[1]).unwrap();
        assert!(child.0.wait().unwrap().success());
    }
    #[test]
    fn growing_or_replaced_source_is_rejected_with_a_declared_length_bound() {
        let temp = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let path = temp.path().join("source");
        fs::write(&path, b"start").unwrap();
        let mut source = Source::open(&path, 10).unwrap();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"growth")
            .unwrap();
        assert!(source.copy_and_hash(None).is_err());
        drop(source);
        fs::write(&path, b"start").unwrap();
        let source = Source::open(&path, 10).unwrap();
        fs::rename(&path, temp.path().join("previous")).unwrap();
        fs::write(&path, b"start").unwrap();
        assert!(source.verify().is_err());
    }
}

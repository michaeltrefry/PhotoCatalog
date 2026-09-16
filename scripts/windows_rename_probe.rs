//! Standalone Windows API probe: rustc only, no image/native dependency build.
use std::{
    ffi::{OsStr, c_void},
    fs::{self, File, OpenOptions},
    io, mem,
    os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle},
    path::Path,
};
#[repr(C)]
struct RenameInfo {
    replace: u32,
    root: *mut c_void,
    length: u32,
    name: [u16; 129],
}
#[repr(C)]
struct IoStatus {
    status: usize,
    information: usize,
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetFileInformationByHandle(
        file: *mut c_void,
        class: i32,
        info: *const c_void,
        length: u32,
    ) -> i32;
}
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtSetInformationFile(
        file: *mut c_void,
        status: *mut IoStatus,
        info: *const c_void,
        length: u32,
        class: i32,
    ) -> i32;
    fn RtlNtStatusToDosError(status: i32) -> u32;
}
fn held(path: &Path) -> io::Result<File> {
    // Omit FILE_FLAG_OVERLAPPED so NtSetInformationFile completes
    // synchronously before its stack-backed IO_STATUS_BLOCK is released.
    OpenOptions::new()
        .read(true)
        .access_mode(0x80000000 | 0x10000 | 0x20)
        .share_mode(1 | 2 | 4)
        .custom_flags(0x02000000 | 0x00200000)
        .open(path)
}
fn rename(source: &File, parent: &File, name: &OsStr, native: bool) -> io::Result<()> {
    let units: Vec<u16> = name.encode_wide().collect();
    assert!(!units.is_empty() && units.len() <= 128 && !units.contains(&0));
    let mut info = RenameInfo {
        replace: 0,
        root: parent.as_raw_handle(),
        length: (units.len() * 2) as u32,
        name: [0; 129],
    };
    info.name[..units.len()].copy_from_slice(&units);
    // x64 SDK sizeof(FILE_RENAME_INFO) is 24, filename offset is 20.
    let bytes = (24 + units.len() * 2) as u32;
    assert!(mem::size_of::<usize>() == 8 && bytes as usize <= mem::size_of_val(&info));
    if native {
        let mut status = IoStatus {
            status: usize::MAX,
            information: 0,
        };
        let code = unsafe {
            NtSetInformationFile(
                source.as_raw_handle(),
                &mut status,
                (&info as *const RenameInfo).cast(),
                bytes,
                10,
            )
        };
        let completion = status.status as i32;
        println!(
            "NtSetInformationFile returned=0x{:08x} completion=0x{:08x} information={}",
            code as u32, completion as u32, status.information
        );
        if code == 0x103 {
            return Err(io::Error::other(
                "NtSetInformationFile unexpectedly returned STATUS_PENDING for a synchronous handle",
            ));
        }
        if code != 0 {
            return Err(io::Error::from_raw_os_error(
                unsafe { RtlNtStatusToDosError(code) } as i32,
            ));
        }
        if completion != 0 {
            return Err(io::Error::from_raw_os_error(
                unsafe { RtlNtStatusToDosError(completion) } as i32,
            ));
        }
    } else {
        let ok = unsafe {
            SetFileInformationByHandle(
                source.as_raw_handle(),
                3,
                (&info as *const RenameInfo).cast(),
                bytes,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
fn scenario(
    base: &Path,
    tag: &str,
    name: &str,
    native: bool,
    move_parent: bool,
    collision: bool,
) -> io::Result<()> {
    let parent_path = base.join(tag);
    fs::create_dir(&parent_path)?;
    let source_path = parent_path.join("source");
    fs::create_dir(&source_path)?;
    fs::write(source_path.join("source-marker"), b"original")?;
    if collision {
        fs::create_dir(parent_path.join(name))?;
        fs::write(parent_path.join(name).join("target-marker"), b"retained")?;
    }
    let parent = held(&parent_path)?;
    let (final_parent, replacement_parent) = if move_parent {
        let moved = base.join(format!("{tag}-moved"));
        // Windows rejects moving this ancestor after a descendant handle is
        // open, even when the handles share delete access. Move the held root
        // first, replace its old pathname, then acquire the exact source under
        // the moved root. Both handles are live at the tested rename call.
        fs::rename(&parent_path, &moved)?;
        fs::create_dir(&parent_path)?;
        fs::create_dir(parent_path.join("source"))?;
        fs::write(parent_path.join("source/substitute-marker"), b"replacement")?;
        (moved, Some(parent_path))
    } else {
        (parent_path, None)
    };
    let source = held(&final_parent.join("source"))?;
    let result = rename(&source, &parent, OsStr::new(name), native);
    println!(
        "case={tag} native={native} parent_moved={move_parent} collision={collision} result={result:?}"
    );
    if collision {
        assert!(result.is_err(), "must not replace existing target");
        assert_eq!(
            fs::read(final_parent.join("source/source-marker"))?,
            b"original"
        );
        assert_eq!(
            fs::read(final_parent.join(name).join("target-marker"))?,
            b"retained"
        );
    } else {
        result?;
        assert!(!final_parent.join("source").exists());
        assert_eq!(
            fs::read(final_parent.join(name).join("source-marker"))?,
            b"original"
        );
    }
    if let Some(replacement) = replacement_parent {
        assert_eq!(
            fs::read(replacement.join("source/substitute-marker"))?,
            b"replacement"
        );
        assert!(!replacement.join(name).exists());
    }
    Ok(())
}
fn main() -> io::Result<()> {
    let base = std::env::temp_dir().join(format!("lensworks-rename-probe-{}", std::process::id()));
    fs::create_dir(&base)?;
    println!(
        "os={} arch={} root={}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        base.display()
    );
    let baseline = scenario(&base, "win32-relative", "q", false, false, false);
    println!("Win32 baseline observation: {baseline:?}");
    let cases = [
        ("nt-short", "q".to_owned(), false, false),
        ("nt-max", "x".repeat(128), false, false),
        ("nt-moved-parent", "r".to_owned(), true, false),
        (
            "nt-moved-parent-collision",
            "existing".to_owned(),
            true,
            true,
        ),
        ("nt-collision", "existing".to_owned(), false, true),
    ];
    let mut failed = false;
    for (tag, name, moved, collision) in cases {
        if let Err(error) = scenario(&base, tag, &name, true, moved, collision) {
            println!("FAILED {tag}: {error}");
            failed = true;
        }
    }
    fs::remove_dir_all(&base)?;
    if failed {
        Err(io::Error::other("native held-relative rename probe failed"))
    } else {
        Ok(())
    }
}

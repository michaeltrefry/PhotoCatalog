//! URI preparation only after original Source/no-link admission and before
//! closed-roster SQLite open. This performs no canonicalizing filesystem open.
//! Reservations are incremental and remain owned by the operation through reap.
use anyhow::{Context, Result, ensure};
use std::path::Path;

type Admit<'a> = &'a mut dyn FnMut(usize) -> Result<()>;
#[cfg(windows)]
fn product(a: usize, b: usize) -> Result<usize> {
    a.checked_mul(b)
        .context("source URI allocation size overflow")
}
fn sum(a: usize, b: usize) -> Result<usize> {
    a.checked_add(b)
        .context("source URI allocation size overflow")
}
fn escape(bytes: &[u8], admit: Admit<'_>) -> Result<String> {
    const PREFIX: &str = "file:";
    const SUFFIX: &str = "?mode=ro&immutable=1";
    let plain = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'/' | b':' | b'-' | b'_' | b'.');
    let length = bytes.iter().try_fold(PREFIX.len() + SUFFIX.len(), |n, b| {
        sum(n, if plain(*b) { 1 } else { 3 })
    })?;
    // Include the subsequent rusqlite CString while this URI still lives.
    admit(sum(length, sum(length, 1)?)?)?;
    let mut output = String::with_capacity(length);
    output.push_str(PREFIX);
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for b in bytes {
        if plain(*b) {
            output.push(*b as char);
        } else {
            output.push('%');
            output.push(HEX[usize::from(b >> 4)] as char);
            output.push(HEX[usize::from(b & 15)] as char);
        }
    }
    output.push_str(SUFFIX);
    debug_assert_eq!(output.len(), length);
    Ok(output)
}

#[cfg(unix)]
pub(super) fn prepare(path: &Path, admit: Admit<'_>) -> Result<String> {
    use std::{os::unix::ffi::OsStrExt, path::Component};
    ensure!(path.is_absolute(), "sealed database path must be absolute");
    let original = path.as_os_str().as_bytes();
    ensure!(!original.contains(&0), "source URI path contains NUL");
    admit(original.len())?;
    let mut normalized = Vec::with_capacity(original.len());
    normalized.push(b'/');
    // Original component validation has already required every traversed
    // directory to exist and not be a link. Never normalize before that check.
    for part in path.components() {
        match part {
            Component::RootDir | Component::CurDir => (),
            Component::ParentDir => {
                if normalized.len() > 1 {
                    let end = normalized.iter().rposition(|b| *b == b'/').unwrap();
                    normalized.truncate(end.max(1));
                }
            }
            Component::Normal(part) => {
                if normalized.len() > 1 {
                    normalized.push(b'/');
                }
                normalized.extend_from_slice(part.as_bytes());
            }
            Component::Prefix(_) => anyhow::bail!("unexpected Unix source prefix"),
        }
    }
    ensure!(
        normalized.len() <= original.len(),
        "source normalization exceeded input"
    );
    escape(&normalized, admit)
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::ptr;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFullPathNameW(
            input: *const u16,
            size: u32,
            output: *mut u16,
            file_part: *mut *mut u16,
        ) -> u32;
    }
    const VERBATIM: &[u16] = &[92, 92, 63, 92];
    const NT: &[u16] = &[92, 63, 63, 92];
    const UNC: &[u16] = &[92, 92, 63, 92, 85, 78, 67, 92];
    fn verbatim(path: &[u16]) -> bool {
        path.starts_with(VERBATIM) || path.starts_with(NT)
    }
    fn normalize(admit: Admit<'_>, mut call: impl FnMut(u32, *mut u16) -> u32) -> Result<Vec<u16>> {
        // The required-size result includes the terminal NUL. No inference from
        // input length and no automatic uncharged growth after the second call.
        let required = call(0, ptr::null_mut());
        ensure!(
            required != 0,
            "Windows source path size query: {}",
            std::io::Error::last_os_error()
        );
        // Rust's pinned fill_utf16_buf uses a 512-unit stack buffer because
        // GetFullPathNameW can report incorrect hints for some short paths.
        // Keep that workaround without making 512 a compatibility ceiling.
        let mut stack = [0u16; 512];
        let mut allocated;
        let capacity = usize::try_from(required)?.max(stack.len());
        let output = if capacity == stack.len() {
            stack.as_mut_ptr()
        } else {
            admit(product(capacity, 2)?)?;
            allocated = Vec::<u16>::with_capacity(capacity);
            allocated.as_mut_ptr()
        };
        let written = call(u32::try_from(capacity)?, output);
        ensure!(
            written != 0,
            "Windows source path normalization: {}",
            std::io::Error::last_os_error()
        );
        ensure!(
            usize::try_from(written)? < capacity,
            "Windows source path changed during admission; explicit preparation retry required"
        );
        // The successful API call initialized exactly this prefix. Capacity is
        // never exposed as initialized u16 storage before that successful call.
        let absolute = unsafe { std::slice::from_raw_parts(output, usize::try_from(written)?) };
        ensure!(
            !absolute.contains(&0),
            "Windows normalized source path contains NUL"
        );
        // Pinned std get_long_path rules with prefer_verbatim=true. Ordinary
        // normalization precedes prefixing; existing verbatim input bypasses it.
        let (prefix, skip): (&[u16], usize) = match absolute {
            [_, 58, 92, ..] => (VERBATIM, 0),
            [92, 92, 46, 92, ..] => (VERBATIM, 4),
            p if verbatim(p) => (&[], 0),
            [92, 92, ..] => (UNC, 2),
            _ => (&[], 0),
        };
        let count = sum(prefix.len(), absolute.len() - skip)?;
        admit(product(count, 2)?)?;
        let mut normalized = Vec::with_capacity(count);
        normalized.extend_from_slice(prefix);
        normalized.extend_from_slice(&absolute[skip..]);
        Ok(normalized)
    }
    fn wide(path: &Path, admit: Admit<'_>) -> Result<Vec<u16>> {
        use std::os::windows::ffi::OsStrExt;
        ensure!(path.is_absolute(), "sealed database path must be absolute");
        let count = path.as_os_str().encode_wide().count();
        admit(product(sum(count, 1)?, 2)?)?;
        let mut input = Vec::with_capacity(sum(count, 1)?);
        input.extend(path.as_os_str().encode_wide());
        ensure!(!input.contains(&0), "source URI path contains NUL");
        input.push(0);
        if verbatim(&input) {
            input.pop();
            Ok(input)
        } else {
            normalize(admit, |size, buffer| unsafe {
                GetFullPathNameW(input.as_ptr(), size, buffer, ptr::null_mut())
            })
        }
    }
    pub(crate) fn file_path(path: &Path, admit: Admit<'_>) -> Result<std::path::PathBuf> {
        use std::{ffi::OsString, os::windows::ffi::OsStringExt};
        let wide = wide(path, admit)?;
        // Pinned Wtf8Buf::from_wide starts at N and pushes <=3N bytes.
        // Doubling can request <=max(8,6N), with an old <=3N allocation
        // during realloc. Preserve surrogate units: this is not SQLite UTF-8.
        let payload = product(wide.len(), 3)?;
        admit(sum(payload, product(payload, 2)?.max(8))?)?;
        let path = OsString::from_wide(&wide);
        // std to_u16s allocates (encoded OsStr byte length + 1) u16s.
        // Verbatim get_long_path returns that vector without OS normalization.
        // This reusable scratch allowance remains retained through all rechecks.
        admit(product(sum(path.len(), 1)?, 2)?)?;
        Ok(path.into())
    }
    pub(crate) fn prepare(path: &Path, admit: Admit<'_>) -> Result<String> {
        let wide = wide(path, admit)?;
        // Count UTF-8 without allocating; preserve SQLite's surrogate rejection.
        let length = char::decode_utf16(wide.iter().copied()).try_fold(0usize, |n, c| {
            sum(
                n,
                c.context("SQLite URI cannot represent an unpaired Windows surrogate")?
                    .len_utf8(),
            )
        })?;
        admit(length)?;
        let mut utf8 = String::with_capacity(length);
        for c in char::decode_utf16(wide.iter().copied()) {
            utf8.push(c?);
        }
        escape(utf8.as_bytes(), admit)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::{
            cell::Cell,
            ffi::OsString,
            os::windows::ffi::{OsStrExt, OsStringExt},
        };
        #[test]
        fn windows_source_uri_preserves_verbatim_and_normalizes_long_drive() -> Result<()> {
            let long = format!("C:\\{}\\..\\é.sqlite", "directory\\".repeat(40));
            let mut admitted = 0usize;
            let uri = prepare(Path::new(&long), &mut |n| {
                admitted += n;
                Ok(())
            })?;
            let expected = format!(r"\\?\C:\{}é.sqlite", "directory\\".repeat(39));
            assert_eq!(uri, escape(expected.as_bytes(), &mut |_| Ok(()))?);
            let unc = prepare(Path::new(r"\\server\share\dir\..\é.sqlite"), &mut |_| {
                Ok(())
            })?;
            assert_eq!(
                unc,
                escape(r"\\?\UNC\server\share\é.sqlite".as_bytes(), &mut |_| Ok(()))?
            );
            assert!(admitted > uri.len());
            for verbatim in [r"\\?\C:\dir\..\é.sqlite", r"\\?\UNC\server\share\name. "] {
                let actual = prepare(Path::new(verbatim), &mut |_| Ok(()))?;
                assert_eq!(actual, escape(verbatim.as_bytes(), &mut |_| Ok(()))?);
            }
            let bad = OsString::from_wide(&[92, 92, 63, 92, 67, 58, 92, 0xd800]);
            let retained = file_path(Path::new(&bad), &mut |_| Ok(()))?;
            assert_eq!(
                retained.as_os_str().encode_wide().collect::<Vec<_>>(),
                bad.encode_wide().collect::<Vec<_>>()
            );
            assert!(prepare(Path::new(&bad), &mut |_| Ok(())).is_err());
            Ok(())
        }

        #[test]
        fn windows_source_uri_short_hint_uses_stack_without_retry() -> Result<()> {
            let calls = Cell::new(0);
            let expected: Vec<u16> = r"C:\é.sqlite".encode_utf16().collect();
            let mut charged = 0;
            let value = normalize(
                &mut |n| {
                    charged += n;
                    Ok(())
                },
                |size, output| {
                    calls.set(calls.get() + 1);
                    if size == 0 {
                        return 2;
                    } // deliberately inaccurate short hint
                    assert_eq!(size, 512);
                    unsafe {
                        std::ptr::copy_nonoverlapping(expected.as_ptr(), output, expected.len());
                    }
                    expected.len().try_into().unwrap()
                },
            )?;
            let normalized: Vec<u16> = r"\\?\C:\é.sqlite".encode_utf16().collect();
            assert_eq!(value, normalized);
            assert_eq!(charged, normalized.len() * 2); // output only; stack has no heap charge
            assert_eq!(calls.get(), 2);
            Ok(())
        }
        #[test]
        fn windows_source_uri_reserves_required_size_before_second_call_and_never_grows() {
            let calls = Cell::new(0);
            let result = normalize(&mut |_| anyhow::bail!("no allowance"), |_, _| {
                calls.set(calls.get() + 1);
                1024
            });
            assert!(result.is_err());
            assert_eq!(calls.get(), 1);
            let calls = Cell::new(0);
            let result = normalize(&mut |_| Ok(()), |size, _| {
                calls.set(calls.get() + 1);
                if size == 0 { 1024 } else { 2048 }
            });
            assert!(result.is_err());
            assert_eq!(calls.get(), 2);
        }
    }
}
#[cfg(windows)]
pub(super) use windows::{file_path, prepare};

#[cfg(windows)]
pub(super) struct Companions(Vec<std::path::PathBuf>);
#[cfg(windows)]
impl Companions {
    pub(crate) fn prepare(path: &Path, admit: Admit<'_>) -> Result<Self> {
        use std::ffi::OsString;
        admit(product(3, std::mem::size_of::<std::path::PathBuf>())?)?;
        let mut paths = Vec::with_capacity(3);
        for suffix in ["-wal", "-shm", "-journal"] {
            let capacity = sum(path.as_os_str().len(), suffix.len())?;
            admit(capacity)?;
            let mut original = OsString::with_capacity(capacity);
            original.push(path);
            original.push(suffix);
            let prepared = file_path(Path::new(&original), admit)?;
            // Preserve the old ordering: absence is checked before Source opens.
            Self::absent(&prepared)?;
            paths.push(prepared);
        }
        Ok(Self(paths))
    }
    fn absent(path: &Path) -> Result<()> {
        match std::fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
            Ok(_) => anyhow::bail!("sealed inspection source has a SQLite companion"),
        }
    }
    pub(super) fn verify(&self) -> Result<()> {
        for path in &self.0 {
            Self::absent(path)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_uri_reservation_denies_before_output_and_escaping_is_exact() -> Result<()> {
        assert!(escape(b"abc", &mut |_| anyhow::bail!("denied")).is_err());
        let mut bytes = 0;
        assert_eq!(
            escape(b"a #\0\xff", &mut |n| {
                bytes = n;
                Ok(())
            })?,
            "file:a%20%23%00%FF?mode=ro&immutable=1"
        );
        assert_eq!(
            bytes,
            2 * "file:a%20%23%00%FF?mode=ro&immutable=1".len() + 1
        );
        Ok(())
    }
}

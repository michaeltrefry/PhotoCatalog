//! Metadata-only, bounded extraction of original XMP carriers.
//!
//! `Packet.bytes` is the exact carrier payload at `ranges` (including carrier
//! headers or compression). `ParseInput.bytes` is separately labelled XML input;
//! it is never substituted for the retained source. No XML normalization occurs.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct Limits {
    pub max_source_bytes: u64,
    pub max_packet_bytes: usize,
    pub max_retained_bytes: usize,
    pub max_parse_bytes: usize,
    pub max_metadata_read_bytes: u64,
    pub max_entries: usize,
    pub max_packets: usize,
    pub max_depth: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_source_bytes: 16 * 1024 * 1024 * 1024,
            max_packet_bytes: 16 * 1024 * 1024,
            max_retained_bytes: 64 * 1024 * 1024,
            max_parse_bytes: 64 * 1024 * 1024,
            max_metadata_read_bytes: 128 * 1024 * 1024,
            max_entries: 100_000,
            max_packets: 1024,
            max_depth: 32,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    Absent,
    Complete,
    Unsupported,
    Malformed,
    ResourceLimit,
    SourceChanged,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    pub status: Status,
    pub offset: Option<u64>,
    pub message: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ByteRange {
    pub offset: u64,
    pub length: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Container {
    /// Exact Adobe_AdditionalMetadata.xmp typed cell bytes.
    CatalogXmp,
    Sidecar,
    JpegMain,
    JpegExtended,
    TiffTag700,
    PngItxt,
    PsdResource1060,
    WebpXmp,
    BmffMime,
    BmffUuid,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Packet {
    pub container: Container,
    pub bytes: Vec<u8>,
    pub blake3: String,
    pub ranges: Vec<ByteRange>,
    /// Stable within this inspection; links fragments or container directories.
    pub group: String,
    /// Container facts, not parsed RDF properties.
    pub attributes: BTreeMap<String, String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Transformation {
    /// Four-byte big-endian length followed by the complete zlib stream.
    CatalogLengthPrefixedZlib,
    Identity,
    CarrierHeaderRemoved,
    ZlibDecompressed,
    GzipDecompressed,
    ExtentsConcatenated,
    JpegExtendedReassembled,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParseInput {
    pub bytes: Vec<u8>,
    pub blake3: String,
    pub packet_indices: Vec<usize>,
    pub transformation: Transformation,
    pub group: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRevision {
    pub length: u64,
    pub blake3: String,
    pub modified_unix_ns: Option<u128>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Inspection {
    pub revision: SourceRevision,
    pub status: Status,
    pub packets: Vec<Packet>,
    pub parse_inputs: Vec<ParseInput>,
    pub issues: Vec<Issue>,
}

/// Read-only inspection. Initial/final filesystem failures are returned. Parser
/// failures are explicit issues, with already recovered carriers still available.
pub fn inspect(path: &Path, limits: &Limits) -> io::Result<Inspection> {
    inspect_file(path, limits, false)
}
/// Retain a selected sidecar verbatim. Selection/naming and conflict policy belong
/// to the caller. This accepts any encoding and does not assert XML validity.
pub fn inspect_sidecar(path: &Path, limits: &Limits) -> io::Result<Inspection> {
    inspect_file(path, limits, true)
}
fn digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}
fn hash_file(file: &mut (impl Read + Seek), length: u64) -> io::Result<(String, bool)> {
    file.seek(SeekFrom::Start(0))?;
    let mut hash = blake3::Hasher::new();
    let mut buffer = [0u8; 65536];
    let mut remaining = length;
    while remaining != 0 {
        let requested = remaining.min(buffer.len() as u64) as usize;
        let n = file.read(&mut buffer[..requested])?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
        remaining -= n as u64;
    }
    // At most the declared length plus one byte, even for an actively growing file.
    let has_extra = file.read(&mut buffer[..1])? != 0;
    Ok((
        hash.finalize().to_hex().to_string(),
        remaining == 0 && !has_extra,
    ))
}
fn modified(metadata: &fs::Metadata) -> Option<u128> {
    metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|v| v.as_nanos())
}
/// Opens only regular files. Unix flags close the metadata/open race: a FIFO
/// replacement cannot block, and a symlink replacement cannot be followed.
fn open_regular(path: &Path, expected: &fs::Metadata) -> io::Result<File> {
    if !expected.is_file() || expected.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source must be a non-symlink regular file",
        ));
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT: open the reparse point itself, never its target.
        options.custom_flags(0x0020_0000);
    }
    let file = options.open(path)?;
    let actual = file.metadata()?;
    let same = actual.is_file()
        && !actual.file_type().is_symlink()
        && actual.len() == expected.len()
        && modified(&actual) == modified(expected);
    #[cfg(unix)]
    let same = {
        use std::os::unix::fs::MetadataExt;
        same && actual.dev() == expected.dev() && actual.ino() == expected.ino()
    };
    if !same {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source changed or became non-regular while opening",
        ));
    }
    Ok(file)
}
/// Actual bytes returned by reads, excluding seeks and EOF probes that return zero.
/// A failed proof query retains conservative whole-file rehashing. No source is
/// cached across calls. These counts are not RSS or filesystem physical-I/O claims.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReadWork {
    pub verification: String,
    pub whole_file_hash_passes: u32,
    pub hash_bytes: u64,
    pub metadata_bytes: u64,
}
struct Counted<'a> {
    file: &'a mut File,
    bytes: &'a mut u64,
}
impl Read for Counted<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let n = self.file.read(buffer)?;
        *self.bytes += n as u64;
        Ok(n)
    }
}
impl Seek for Counted<'_> {
    fn seek(&mut self, offset: SeekFrom) -> io::Result<u64> {
        self.file.seek(offset)
    }
}
#[derive(Debug, PartialEq, Eq)]
struct StabilityStamp {
    object: (u64, u128),
    bytes: u64,
    modified: std::time::SystemTime,
    changed: i128,
}
fn stability_stamp(file: &File) -> io::Result<StabilityStamp> {
    let meta = file.metadata()?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(io::Error::other("stability handle is not a regular file"));
    }
    #[cfg(unix)]
    let (object, changed) = {
        use std::os::unix::fs::MetadataExt;
        (
            (meta.dev(), u128::from(meta.ino())),
            i128::from(meta.ctime()) * 1_000_000_000 + i128::from(meta.ctime_nsec()),
        )
    };
    #[cfg(windows)]
    let (object, changed) = {
        // The deny-write handle supplies content stability; Windows ChangeTime
        // alone is not a reliable substitute for that lease.
        (crate::storage_volume::held_object_key(file)?, 0)
    };
    #[cfg(not(any(unix, windows)))]
    let (object, changed) = return Err(io::Error::other("held stability unsupported"));
    Ok(StabilityStamp {
        object,
        bytes: meta.len(),
        modified: meta.modified()?,
        changed,
    })
}
// A nanosecond-shaped stat field alone does not establish update precision.
// Linux may stamp once per jiffy; unknown and network filesystems keep rehashing.
#[cfg(target_os = "macos")]
fn qualified_apfs_mount(name: &[u8], local: bool) -> bool {
    local && name == b"apfs"
}
fn qualified_unix_file(_file: &File) -> bool {
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        let mut info = std::mem::MaybeUninit::<libc::statfs>::uninit();
        if unsafe { libc::fstatfs(_file.as_raw_fd(), info.as_mut_ptr()) } != 0 {
            return false;
        }
        let info = unsafe { info.assume_init() };
        let name: Vec<u8> = info.f_fstypename.iter().map(|&c| c as u8).collect();
        let Some(end) = name.iter().position(|&c| c == 0) else {
            return false;
        };
        qualified_apfs_mount(&name[..end], info.f_flags & libc::MNT_LOCAL as u32 != 0)
    }
    #[cfg(not(target_os = "macos"))]
    false
}
fn open_stable(
    path: &Path,
    expected: &fs::Metadata,
    conservative: bool,
) -> io::Result<(File, bool)> {
    #[cfg(windows)]
    if !conservative {
        use std::os::windows::fs::OpenOptionsExt;
        // Same sharing contract as VerifiedFile: hold through hash, parser and
        // final pathname verification. A conflicting writer uses old rehashing.
        // CreateFile documents that excluding FILE_SHARE_WRITE also rejects an
        // existing writable mapping, even after its creator handle is closed:
        // https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilew
        if let Ok(file) = fs::OpenOptions::new()
            .read(true)
            .share_mode(1 | 4)
            .custom_flags(0x0020_0000)
            .open(path)
        {
            let meta = file.metadata()?;
            if !meta.is_file()
                || meta.file_type().is_symlink()
                || meta.len() != expected.len()
                || modified(&meta) != modified(expected)
            {
                return Err(io::Error::other(
                    "source changed while acquiring stability lease",
                ));
            }
            return Ok((file, true));
        }
    }
    let file = open_regular(path, expected)?;
    let eligible = !conservative && qualified_unix_file(&file);
    Ok((file, eligible))
}
/// Inspect with observable verification cost; extraction and revision values are
/// identical to `inspect`. Local macOS APFS relies on its precise ctime,
/// not an atomic snapshot; Windows uses a held deny-write lease and full file ID.
pub fn inspect_with_work(path: &Path, limits: &Limits) -> io::Result<(Inspection, ReadWork)> {
    inspect_observed(path, limits, false, false, &mut |_| Ok(()))
}
pub fn inspect_sidecar_with_work(
    path: &Path,
    limits: &Limits,
) -> io::Result<(Inspection, ReadWork)> {
    inspect_observed(path, limits, true, false, &mut |_| Ok(()))
}
fn inspect_file(path: &Path, limits: &Limits, sidecar: bool) -> io::Result<Inspection> {
    if sidecar {
        inspect_sidecar_with_work(path, limits)
    } else {
        inspect_with_work(path, limits)
    }
    .map(|(inspection, _)| inspection)
}
fn inspect_observed(
    path: &Path,
    limits: &Limits,
    sidecar: bool,
    conservative: bool,
    checkpoint: &mut impl FnMut(&str) -> io::Result<()>,
) -> io::Result<(Inspection, ReadWork)> {
    let path_before = fs::symlink_metadata(path)?;
    let (mut file, eligible) = open_stable(path, &path_before, conservative)?;
    let before = file.metadata()?;
    let proof = stability_stamp(&file).ok();
    let mut work = ReadWork::default();
    let mut revision = SourceRevision {
        length: before.len(),
        blake3: String::new(),
        modified_unix_ns: modified(&before),
    };
    if before.len() > limits.max_source_bytes {
        work.verification = "not_attempted_source_limit".into();
        return Ok((
            Inspection {
                revision,
                status: Status::ResourceLimit,
                packets: vec![],
                parse_inputs: vec![],
                issues: vec![Issue {
                    status: Status::ResourceLimit,
                    offset: None,
                    message: "source byte limit exceeded; digest not computed".into(),
                }],
            },
            work,
        ));
    }
    work.whole_file_hash_passes += 1;
    let (initial_hash, initial_length_matches) = hash_file(
        &mut Counted {
            file: &mut file,
            bytes: &mut work.hash_bytes,
        },
        before.len(),
    )?;
    revision.blake3 = initial_hash;
    checkpoint("after_hash")?;
    let hash_stamp = if proof.is_some() {
        stability_stamp(&file).ok()
    } else {
        None
    };
    let mut changed = proof
        .as_ref()
        .zip(hash_stamp.as_ref())
        .is_some_and(|(a, b)| a != b);
    let mut counted = Counted {
        file: &mut file,
        bytes: &mut work.metadata_bytes,
    };
    let mut parser = Parser::new(&mut counted, before.len(), limits);
    let parsed = if sidecar {
        parser.sidecar()
    } else {
        parser.photo()
    };
    if let Err(failure) = parsed {
        parser.record(failure);
    }
    let (packets, parse_inputs, mut issues) = (parser.packets, parser.inputs, parser.issues);
    checkpoint("after_parse")?;
    let after_stamp = if proof.is_some() {
        stability_stamp(&file).ok()
    } else {
        None
    };
    let current = fs::symlink_metadata(path)?;
    let path_stamp = if proof.is_some() {
        open_regular(path, &current)
            .and_then(|f| stability_stamp(&f))
            .ok()
    } else {
        None
    };
    changed |= proof
        .as_ref()
        .zip(after_stamp.as_ref())
        .is_some_and(|(a, b)| a != b)
        || proof
            .as_ref()
            .zip(path_stamp.as_ref())
            .is_some_and(|(a, b)| a != b);
    let established = eligible
        && proof.is_some()
        && hash_stamp.is_some()
        && after_stamp.is_some()
        && path_stamp.is_some();
    let (after_hash, final_length_matches) = if established {
        work.verification = if cfg!(windows) {
            "held_deny_write_file_id"
        } else {
            "held_object_local_apfs_change_stamp"
        }
        .into();
        (revision.blake3.clone(), true)
    } else {
        work.verification = "conservative_full_rehash".into();
        work.whole_file_hash_passes += 1;
        hash_file(
            &mut Counted {
                file: &mut file,
                bytes: &mut work.hash_bytes,
            },
            before.len(),
        )?
    };
    if !established {
        checkpoint("after_fallback_hash")?;
    }
    let after = file.metadata()?;
    // The conservative path must sample the pathname after its final hash too.
    let current = fs::symlink_metadata(path)?;
    if !established {
        // Preserve any known change evidence through the entire fallback scan.
        let final_stamp = stability_stamp(&file).ok();
        let final_path_stamp = open_regular(path, &current)
            .and_then(|f| stability_stamp(&f))
            .ok();
        changed |= proof
            .as_ref()
            .zip(final_stamp.as_ref())
            .is_some_and(|(a, b)| a != b)
            || proof
                .as_ref()
                .zip(final_path_stamp.as_ref())
                .is_some_and(|(a, b)| a != b);
    }
    let mut same = !changed
        && initial_length_matches
        && final_length_matches
        && !current.file_type().is_symlink()
        && path_before.len() == before.len()
        && modified(&path_before) == modified(&before)
        && before.len() == after.len()
        && modified(&before) == modified(&after)
        && revision.blake3 == after_hash
        && current.len() == after.len()
        && modified(&current) == modified(&after);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        same &= before.dev() == current.dev()
            && before.ino() == current.ino()
            && before.dev() == path_before.dev()
            && before.ino() == path_before.ino();
    }
    #[cfg(not(unix))]
    if same && !established {
        let mut current_file = open_regular(path, &current)?;
        work.whole_file_hash_passes += 1;
        let (path_hash, length_matches) = hash_file(
            &mut Counted {
                file: &mut current_file,
                bytes: &mut work.hash_bytes,
            },
            before.len(),
        )?;
        same &= length_matches && path_hash == after_hash;
    }
    if !same {
        issues.push(Issue {
            status: Status::SourceChanged,
            offset: None,
            message: "source changed during inspection; packets are not a committed revision"
                .into(),
        });
    }
    let status = [
        Status::SourceChanged,
        Status::ResourceLimit,
        Status::Malformed,
        Status::Unsupported,
    ]
    .into_iter()
    .find(|status| issues.iter().any(|issue| issue.status == *status))
    .unwrap_or(if packets.is_empty() {
        Status::Absent
    } else {
        Status::Complete
    });
    Ok((
        Inspection {
            revision,
            status,
            packets,
            parse_inputs,
            issues,
        },
        work,
    ))
}

#[derive(Debug)]
struct Failure {
    status: Status,
    offset: u64,
    message: String,
    io: Option<io::Error>,
}
type PResult<T> = Result<T, Failure>;
fn malformed(offset: u64, message: impl Into<String>) -> Failure {
    Failure {
        status: Status::Malformed,
        offset,
        message: message.into(),
        io: None,
    }
}
fn limited(offset: u64, message: impl Into<String>) -> Failure {
    Failure {
        status: Status::ResourceLimit,
        ..malformed(offset, message)
    }
}
fn unsupported(offset: u64, message: impl Into<String>) -> Failure {
    Failure {
        status: Status::Unsupported,
        ..malformed(offset, message)
    }
}
struct Parser<'a, R: Read + Seek> {
    reader: &'a mut R,
    length: u64,
    limits: &'a Limits,
    read_bytes: u64,
    entries: usize,
    retained: usize,
    parsed: usize,
    packets: Vec<Packet>,
    inputs: Vec<ParseInput>,
    issues: Vec<Issue>,
}
impl<'a, R: Read + Seek> Parser<'a, R> {
    fn new(reader: &'a mut R, length: u64, limits: &'a Limits) -> Self {
        Self {
            reader,
            length,
            limits,
            read_bytes: 0,
            entries: 0,
            retained: 0,
            parsed: 0,
            packets: vec![],
            inputs: vec![],
            issues: vec![],
        }
    }
    fn record(&mut self, failure: Failure) {
        let message = if let Some(error) = failure.io {
            format!("{}: {error}", failure.message)
        } else {
            failure.message
        };
        self.issues.push(Issue {
            status: failure.status,
            offset: Some(failure.offset),
            message,
        });
    }
    fn tick(&mut self, offset: u64) -> PResult<()> {
        self.entries += 1;
        if self.entries > self.limits.max_entries {
            return Err(limited(offset, "container entry limit exceeded"));
        }
        Ok(())
    }
    fn bounds(&self, offset: u64, length: u64, end: u64) -> PResult<u64> {
        offset
            .checked_add(length)
            .filter(|v| *v <= end && *v <= self.length)
            .ok_or_else(|| malformed(offset, "container offset/length exceeds source or parent"))
    }
    fn read(&mut self, offset: u64, length: u64) -> PResult<Vec<u8>> {
        self.bounds(offset, length, self.length)?;
        if length > self.limits.max_packet_bytes as u64 {
            return Err(limited(offset, "individual metadata read limit exceeded"));
        }
        self.read_bytes = self
            .read_bytes
            .checked_add(length)
            .ok_or_else(|| limited(offset, "read byte overflow"))?;
        if self.read_bytes > self.limits.max_metadata_read_bytes {
            return Err(limited(offset, "metadata read budget exceeded"));
        }
        let mut bytes = vec![0; length as usize];
        self.reader
            .seek(SeekFrom::Start(offset))
            .and_then(|_| self.reader.read_exact(&mut bytes))
            .map_err(|error| Failure {
                io: Some(error),
                ..malformed(offset, "source read failed")
            })?;
        Ok(bytes)
    }
    fn packet(
        &mut self,
        container: Container,
        bytes: Vec<u8>,
        ranges: Vec<ByteRange>,
        group: String,
        attributes: BTreeMap<String, String>,
    ) -> PResult<usize> {
        let offset = ranges.first().map_or(0, |r| r.offset);
        if self.packets.len() >= self.limits.max_packets
            || bytes.len() > self.limits.max_packet_bytes
            || bytes.len() > self.limits.max_retained_bytes.saturating_sub(self.retained)
        {
            return Err(limited(offset, "retained packet budget exceeded"));
        }
        self.retained += bytes.len();
        let index = self.packets.len();
        self.packets.push(Packet {
            container,
            blake3: digest(&bytes),
            bytes,
            ranges,
            group,
            attributes,
        });
        Ok(index)
    }
    fn input(
        &mut self,
        bytes: Vec<u8>,
        indices: Vec<usize>,
        transformation: Transformation,
        group: String,
    ) -> PResult<()> {
        if bytes.len() > self.limits.max_packet_bytes
            || bytes.len() > self.limits.max_parse_bytes.saturating_sub(self.parsed)
        {
            return Err(limited(0, "parse input budget exceeded"));
        }
        self.parsed += bytes.len();
        self.inputs.push(ParseInput {
            blake3: digest(&bytes),
            bytes,
            packet_indices: indices,
            transformation,
            group,
        });
        Ok(())
    }
    fn simple(
        &mut self,
        container: Container,
        offset: u64,
        length: u64,
        group: String,
    ) -> PResult<()> {
        self.simple_within(container, offset, length, group, self.length)
    }
    fn simple_within(
        &mut self,
        container: Container,
        offset: u64,
        length: u64,
        group: String,
        end: u64,
    ) -> PResult<()> {
        self.bounds(offset, 0, end)?;
        let available = end.min(self.length).saturating_sub(offset).min(length);
        let bytes = self.read(offset, available)?;
        if available != length {
            self.packet(
                container,
                bytes,
                vec![ByteRange {
                    offset,
                    length: available,
                }],
                group,
                attrs(&[
                    ("declared_length", length.to_string()),
                    ("incomplete", "true".into()),
                ]),
            )?;
            return Err(malformed(
                offset,
                "truncated XMP payload; available source bytes retained",
            ));
        }
        let i = self.packet(
            container,
            bytes.clone(),
            vec![ByteRange { offset, length }],
            group.clone(),
            BTreeMap::new(),
        )?;
        self.input(bytes, vec![i], Transformation::Identity, group)
    }
    fn sidecar(&mut self) -> PResult<()> {
        self.simple(Container::Sidecar, 0, self.length, "sidecar".into())
    }
    fn photo(&mut self) -> PResult<()> {
        let header = self.read(0, self.length.min(16))?;
        if header.starts_with(&[0xff, 0xd8]) {
            return self.jpeg(0, self.length);
        }
        if header.starts_with(b"\x89PNG\r\n\x1a\n") {
            return self.png();
        }
        if header.starts_with(b"RIFF") && header.get(8..12) == Some(b"WEBP") {
            return self.webp();
        }
        if header.starts_with(b"8BPS") {
            return self.psd();
        }
        if header.starts_with(b"II") || header.starts_with(b"MM") {
            return self.tiff(0, self.length);
        }
        if header.starts_with(b"FUJIFILMCCD-RAW ") {
            return self.raf();
        }
        if header.get(4..8) == Some(b"ftyp") {
            let ftyp = self.box_at(0, self.length)?;
            {
                let brands = self.read(ftyp.payload, ftyp.end - ftyp.payload)?;
                if brands.len() < 8 || (brands.len() - 8) % 4 != 0 {
                    return Err(malformed(ftyp.start, "invalid BMFF file type box"));
                }
                let supported = [&brands[..4]]
                    .into_iter()
                    .chain(
                        brands[8..]
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .map(|brand| brand.as_slice()),
                    )
                    .any(|brand| matches!(brand, b"avif" | b"avis" | b"crx "));
                if !supported {
                    self.record(unsupported(
                        0,
                        "BMFF brands outside AVIF/CR3; only declared XMP carriers inspected",
                    ));
                }
            }
            return self.bmff(0, self.length, 0);
        }
        if header.starts_with(b"BM") {
            return Err(unsupported(
                0,
                "BMP has no supported standardized embedded XMP carrier; inspect an explicitly selected sidecar separately",
            ));
        }
        Err(unsupported(0, "unrecognized metadata container"))
    }
    fn raf(&mut self) -> PResult<()> {
        let header = self.read(0, 108)?;
        let offset = be32(&header[84..88]) as u64;
        let length = be32(&header[88..92]) as u64;
        self.bounds(offset, length, self.length)?;
        if length != 0 {
            self.jpeg(offset, offset + length)?;
        }
        // RAF metadata outside the embedded JPEG has no supported XMP carrier.
        self.record(unsupported(
            0,
            "RAF embedded JPEG inspected; proprietary RAF metadata is not a documented XMP carrier",
        ));
        Ok(())
    }
}
fn be16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes(bytes[..2].try_into().expect("checked field"))
}
fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes[..4].try_into().expect("checked field"))
}
fn le32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[..4].try_into().expect("checked field"))
}
fn attrs(pairs: &[(&str, String)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

const JPEG_MAIN: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
const JPEG_EXT: &[u8] = b"http://ns.adobe.com/xmp/extension/\0";
struct Extended {
    index: usize,
    total: u32,
    start: u32,
    bytes: Vec<u8>,
}
impl<R: Read + Seek> Parser<'_, R> {
    // Scan entropy data as bounded blocks. FF00 is byte stuffing; restart markers
    // stay inside the scan. All other markers resume the structural parser.
    fn entropy_end(&mut self, mut offset: u64, end: u64) -> PResult<u64> {
        let mut ff = None;
        let mut buffer = [0u8; 65536];
        while offset < end {
            let n = (end - offset).min(buffer.len() as u64) as usize;
            self.reader
                .seek(SeekFrom::Start(offset))
                .and_then(|_| self.reader.read_exact(&mut buffer[..n]))
                .map_err(|error| Failure {
                    io: Some(error),
                    ..malformed(offset, "JPEG scan read failed")
                })?;
            for (i, byte) in buffer[..n].iter().copied().enumerate() {
                let pos = offset + i as u64;
                if let Some(marker) = ff {
                    match byte {
                        0x00 | 0x01 | 0xd0..=0xd7 => ff = None,
                        0xff => {}
                        _ => return Ok(marker),
                    }
                } else if byte == 0xff {
                    ff = Some(pos);
                }
            }
            offset += n as u64;
        }
        Err(malformed(offset, "JPEG scan has no terminating marker"))
    }
    fn jpeg(&mut self, base: u64, end: u64) -> PResult<()> {
        if self.read(base, 2)? != [0xff, 0xd8] {
            return Err(malformed(base, "invalid JPEG SOI"));
        }
        let mut offset = base + 2;
        let mut groups: BTreeMap<String, Vec<Extended>> = BTreeMap::new();
        let result = (|| {
            while offset < end {
                self.tick(offset)?;
                let start = offset;
                if self.read(offset, 1)? != [0xff] {
                    return Err(malformed(offset, "invalid JPEG marker boundary"));
                }
                offset += 1;
                let marker = loop {
                    self.bounds(offset, 1, end)?;
                    let value = self.read(offset, 1)?[0];
                    offset += 1;
                    if value != 0xff {
                        break value;
                    }
                };
                if marker == 0xd9 {
                    return Ok(());
                }
                if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
                    continue;
                }
                if marker == 0x00 || marker == 0xd8 {
                    return Err(malformed(start, "invalid JPEG marker"));
                }
                self.bounds(offset, 2, end)?;
                let length = be16(&self.read(offset, 2)?) as u64;
                if length < 2 {
                    return Err(malformed(offset, "invalid JPEG segment length"));
                }
                let payload = offset + 2;
                let next = match self.bounds(offset, length, end) {
                    Ok(next) => next,
                    Err(error) => {
                        if marker == 0xe1 && payload <= end {
                            let bytes = self.read(payload, end - payload)?;
                            let container = if bytes.starts_with(JPEG_MAIN) {
                                Some(Container::JpegMain)
                            } else if bytes.starts_with(JPEG_EXT) {
                                Some(Container::JpegExtended)
                            } else {
                                None
                            };
                            if let Some(container) = container {
                                self.packet(
                                    container,
                                    bytes,
                                    vec![ByteRange {
                                        offset: payload,
                                        length: end - payload,
                                    }],
                                    format!("jpeg:{base}:truncated:{payload}"),
                                    attrs(&[
                                        ("declared_length", (length - 2).to_string()),
                                        ("incomplete", "true".into()),
                                    ]),
                                )?;
                            }
                        }
                        return Err(error);
                    }
                };
                if marker == 0xe1 {
                    let bytes = self.read(payload, length - 2)?;
                    if bytes.starts_with(JPEG_MAIN) {
                        let group = format!("jpeg:{base}:main:{payload}");
                        let xml = bytes[JPEG_MAIN.len()..].to_vec();
                        let i = self.packet(
                            Container::JpegMain,
                            bytes,
                            vec![ByteRange {
                                offset: payload,
                                length: length - 2,
                            }],
                            group.clone(),
                            BTreeMap::new(),
                        )?;
                        self.input(xml, vec![i], Transformation::CarrierHeaderRemoved, group)?;
                    } else if bytes.starts_with(JPEG_EXT) {
                        let header = JPEG_EXT.len();
                        let group = if bytes.len() >= header + 32 {
                            String::from_utf8_lossy(&bytes[header..header + 32]).into_owned()
                        } else {
                            "invalid-guid".into()
                        };
                        let i = self.packet(
                            Container::JpegExtended,
                            bytes.clone(),
                            vec![ByteRange {
                                offset: payload,
                                length: length - 2,
                            }],
                            format!("jpeg:{base}:extended:{group}"),
                            BTreeMap::new(),
                        )?;
                        if bytes.len() < header + 40
                            || !group.as_bytes().iter().all(u8::is_ascii_hexdigit)
                        {
                            self.record(malformed(payload, "invalid extended JPEG GUID/header"));
                        } else {
                            let total = be32(&bytes[header + 32..header + 36]);
                            let start = be32(&bytes[header + 36..header + 40]);
                            self.packets[i].attributes = attrs(&[
                                ("guid", group.clone()),
                                ("full_length", total.to_string()),
                                ("fragment_offset", start.to_string()),
                            ]);
                            groups
                                .entry(group.to_ascii_uppercase())
                                .or_default()
                                .push(Extended {
                                    index: i,
                                    total,
                                    start,
                                    bytes: bytes[header + 40..].to_vec(),
                                });
                        }
                    }
                }
                offset = if marker == 0xda || marker == 0xdc {
                    self.entropy_end(next, end)?
                } else {
                    next
                };
            }
            Err(malformed(offset, "JPEG missing EOI"))
        })();
        // Reassemble valid groups even if an unrelated later segment is broken.
        for (guid, mut fragments) in groups {
            fragments.sort_by_key(|f| f.start);
            let total = fragments[0].total;
            let mut position = 0u64;
            let valid = fragments.iter().all(|f| {
                let matches =
                    !f.bytes.is_empty() && f.total == total && u64::from(f.start) == position;
                position += f.bytes.len() as u64;
                matches && position <= u64::from(total)
            }) && position == u64::from(total);
            if !valid {
                self.record(malformed(
                    base,
                    format!(
                        "extended JPEG {guid}: gap, overlap, duplicate, or inconsistent length"
                    ),
                ));
                continue;
            }
            if total as usize > self.limits.max_packet_bytes {
                self.record(limited(base, "extended JPEG reconstruction limit exceeded"));
                continue;
            }
            let bytes: Vec<u8> = fragments
                .iter()
                .flat_map(|f| f.bytes.iter().copied())
                .collect();
            if format!("{:X}", md5::compute(&bytes)) != guid {
                self.record(malformed(
                    base,
                    format!("extended JPEG {guid}: MD5 mismatch"),
                ));
                continue;
            }
            self.input(
                bytes,
                fragments.iter().map(|f| f.index).collect(),
                Transformation::JpegExtendedReassembled,
                format!("jpeg:{base}:extended:{guid}"),
            )?;
        }
        result
    }
    fn png(&mut self) -> PResult<()> {
        let mut offset = 8;
        let mut ended = false;
        while offset < self.length {
            self.tick(offset)?;
            let header = self.read(offset, 8)?;
            let length = be32(&header[..4]) as u64;
            let complete = self.bounds(offset, length + 12, self.length);
            if &header[4..8] == b"iTXt" {
                let available = length.min(self.length.saturating_sub(offset + 8));
                let bytes = self.read(offset + 8, available)?;
                let keyword_end = bytes.iter().position(|b| *b == 0);
                if keyword_end.map(|i| &bytes[..i]) == Some(b"XML:com.adobe.xmp".as_slice())
                    || bytes == b"XML:com.adobe.xmp"
                {
                    let group = format!("png:itxt:{offset}");
                    let i = self.packet(
                        Container::PngItxt,
                        bytes.clone(),
                        vec![ByteRange {
                            offset: offset + 8,
                            length: available,
                        }],
                        group.clone(),
                        BTreeMap::new(),
                    )?;
                    if complete.is_err() || keyword_end.is_none() {
                        self.packets[i].attributes = attrs(&[
                            ("declared_length", length.to_string()),
                            ("incomplete", "true".into()),
                        ]);
                        return Err(malformed(
                            offset,
                            "truncated XMP iTXt carrier or missing keyword terminator",
                        ));
                    }
                    let expected_crc = be32(&self.read(offset + 8 + length, 4)?);
                    let mut crc_bytes = b"iTXt".to_vec();
                    crc_bytes.extend_from_slice(&bytes);
                    if crc32(&crc_bytes) != expected_crc {
                        self.record(malformed(offset, "XMP iTXt CRC mismatch"));
                        offset = complete?;
                        continue;
                    }
                    let parsed =
                        self.png_text(&bytes, keyword_end.expect("matched keyword") + 1, offset);
                    match parsed {
                        Ok((xml, transform)) => self.input(xml, vec![i], transform, group)?,
                        Err(error) => self.record(error),
                    }
                }
            }
            offset = complete?;
            if &header[4..8] == b"IEND" {
                if length != 0 {
                    return Err(malformed(offset, "PNG IEND has payload"));
                }
                ended = true;
                break;
            }
        }
        if !ended || offset != self.length {
            return Err(malformed(offset, "PNG missing IEND or trailing bytes"));
        }
        Ok(())
    }
    fn png_text(
        &self,
        bytes: &[u8],
        start: usize,
        offset: u64,
    ) -> PResult<(Vec<u8>, Transformation)> {
        let mut cursor = Slice::new(&bytes[start..], offset);
        let flag = cursor.uint(1)?;
        let method = cursor.uint(1)?;
        if flag > 1 || method != 0 {
            return Err(malformed(offset, "invalid XMP iTXt compression fields"));
        }
        cursor.nul()?; // language tag, retained unchanged in carrier
        cursor.nul()?; // translated keyword
        let text = cursor.remaining();
        if flag == 0 {
            return Ok((text.to_vec(), Transformation::CarrierHeaderRemoved));
        }
        let mut decoder = flate2::read::ZlibDecoder::new(text);
        let mut xml = Vec::new();
        decoder
            .by_ref()
            .take(self.limits.max_packet_bytes as u64 + 1)
            .read_to_end(&mut xml)
            .map_err(|_| malformed(offset, "invalid compressed XMP iTXt"))?;
        if xml.len() > self.limits.max_packet_bytes {
            return Err(limited(offset, "decompressed XMP limit exceeded"));
        }
        if decoder.total_in() != text.len() as u64 {
            return Err(malformed(offset, "trailing compressed XMP bytes"));
        }
        Ok((xml, Transformation::ZlibDecompressed))
    }
    fn webp(&mut self) -> PResult<()> {
        let header = self.read(0, 12)?;
        let end = u64::from(le32(&header[4..8])) + 8;
        if end != self.length {
            self.record(malformed(4, "RIFF declared size differs from source"));
        }
        let end = end.min(self.length);
        let mut offset = 12;
        while offset < end {
            self.tick(offset)?;
            self.bounds(offset, 8, end)?;
            let header = self.read(offset, 8)?;
            let length = le32(&header[4..8]) as u64;
            if &header[..4] == b"XMP " {
                self.simple_within(
                    Container::WebpXmp,
                    offset + 8,
                    length,
                    format!("webp:{offset}"),
                    end,
                )?;
            }
            offset = self.bounds(offset + 8, length + length % 2, end)?;
        }
        Ok(())
    }
    fn psd(&mut self) -> PResult<()> {
        let header = self.read(0, 26)?;
        if be16(&header[4..6]) != 1 {
            return Err(unsupported(4, "only PSD version 1 is supported"));
        }
        let color_length = be32(&self.read(26, 4)?) as u64;
        let resource_start = self.bounds(30, color_length, self.length)?;
        let resource_length = be32(&self.read(resource_start, 4)?) as u64;
        let end = match self.bounds(resource_start + 4, resource_length, self.length) {
            Ok(end) => end,
            Err(error) => {
                self.record(error);
                self.length
            }
        };
        let mut offset = resource_start + 4;
        while offset < end {
            self.tick(offset)?;
            self.bounds(offset, 7, end)?;
            let header = self.read(offset, 7)?;
            let standard_resource = &header[..4] == b"8BIM";
            // AgHg uses the same bounded framing, but its private resource IDs
            // do not share the standard 8BIM XMP namespace.
            if !standard_resource && &header[..4] != b"AgHg" {
                return Err(malformed(offset, "invalid PSD resource signature"));
            }
            let name_length = u64::from(header[6]) + 1;
            let size_offset = self.bounds(offset + 6, name_length + name_length % 2, end)?;
            self.bounds(size_offset, 4, end)?;
            let length = be32(&self.read(size_offset, 4)?) as u64;
            let payload = size_offset + 4;
            if standard_resource && be16(&header[4..6]) == 1060 {
                self.simple_within(
                    Container::PsdResource1060,
                    payload,
                    length,
                    format!("psd:{offset}"),
                    end,
                )?;
            }
            offset = self.bounds(payload, length + length % 2, end)?;
        }
        Ok(())
    }
}
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}
struct Slice<'a> {
    bytes: &'a [u8],
    pos: usize,
    offset: u64,
}
impl<'a> Slice<'a> {
    fn new(bytes: &'a [u8], offset: u64) -> Self {
        Self {
            bytes,
            pos: 0,
            offset,
        }
    }
    fn take(&mut self, length: usize) -> PResult<&'a [u8]> {
        let end = self
            .pos
            .checked_add(length)
            .filter(|v| *v <= self.bytes.len())
            .ok_or_else(|| malformed(self.offset, "truncated metadata field"))?;
        let value = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(value)
    }
    fn uint(&mut self, size: usize) -> PResult<u64> {
        if size > 8 {
            return Err(malformed(self.offset, "integer field exceeds 64 bits"));
        }
        Ok(self
            .take(size)?
            .iter()
            .fold(0, |n, b| (n << 8) | u64::from(*b)))
    }
    fn nul(&mut self) -> PResult<&'a [u8]> {
        let length = self
            .remaining()
            .iter()
            .position(|b| *b == 0)
            .ok_or_else(|| malformed(self.offset, "unterminated metadata string"))?;
        let value = self.take(length)?;
        self.take(1)?;
        Ok(value)
    }
    fn remaining(&self) -> &'a [u8] {
        &self.bytes[self.pos..]
    }
}

#[derive(Clone, Copy)]
struct Tiff {
    base: u64,
    end: u64,
    little: bool,
    big: bool,
}
impl Tiff {
    fn uint(self, bytes: &[u8]) -> u64 {
        if self.little {
            bytes.iter().rev().fold(0, |n, b| (n << 8) | u64::from(*b))
        } else {
            bytes.iter().fold(0, |n, b| (n << 8) | u64::from(*b))
        }
    }
    fn absolute(self, offset: u64) -> PResult<u64> {
        self.base
            .checked_add(offset)
            .filter(|v| *v <= self.end)
            .ok_or_else(|| malformed(self.base, "TIFF relative offset overflow"))
    }
}
impl<R: Read + Seek> Parser<'_, R> {
    fn tiff(&mut self, base: u64, end: u64) -> PResult<()> {
        let header = self.read(base, 8)?;
        let little = &header[..2] == b"II";
        if !little && &header[..2] != b"MM" {
            return Err(malformed(base, "invalid TIFF byte order"));
        }
        let mut tiff = Tiff {
            base,
            end,
            little,
            big: false,
        };
        let magic = tiff.uint(&header[2..4]);
        tiff.big = magic == 43;
        if !matches!(magic, 42 | 43 | 85 | 0x4f52 | 0x5352) {
            return Err(unsupported(base + 2, "unsupported TIFF-family magic"));
        }
        let first = if tiff.big {
            if tiff.uint(&header[4..6]) != 8 || tiff.uint(&header[6..8]) != 0 {
                return Err(malformed(base + 4, "invalid BigTIFF header"));
            }
            tiff.uint(&self.read(base + 8, 8)?)
        } else {
            tiff.uint(&header[4..8])
        };
        let mut active = BTreeSet::new();
        let mut visited = BTreeSet::new();
        self.tiff_ifd(tiff, first, 0, &mut active, &mut visited)
    }
    fn tiff_ifd(
        &mut self,
        tiff: Tiff,
        relative: u64,
        depth: usize,
        active: &mut BTreeSet<u64>,
        visited: &mut BTreeSet<u64>,
    ) -> PResult<()> {
        if relative == 0 {
            return Ok(());
        }
        if depth > self.limits.max_depth {
            return Err(limited(tiff.base, "TIFF directory depth exceeded"));
        }
        let offset = tiff.absolute(relative)?;
        if active.contains(&offset) {
            return Err(malformed(offset, "TIFF directory cycle"));
        }
        if !visited.insert(offset) {
            return Ok(());
        } // shared directory, not a cycle
        active.insert(offset);
        let result = (|| {
            self.tick(offset)?;
            let count_size = if tiff.big { 8 } else { 2 };
            let entry_size = if tiff.big { 20 } else { 12 };
            let slot_size = if tiff.big { 8 } else { 4 };
            self.bounds(offset, count_size, tiff.end)?;
            let count = tiff.uint(&self.read(offset, count_size)?);
            if count > self.limits.max_entries as u64 {
                return Err(limited(offset, "TIFF directory entry count exceeded"));
            }
            let start = offset + count_size;
            let next_offset = self.bounds(
                start,
                count
                    .checked_mul(entry_size)
                    .ok_or_else(|| malformed(offset, "TIFF directory size overflow"))?,
                tiff.end,
            )?;
            self.bounds(next_offset, slot_size, tiff.end)?;
            let mut children = Vec::new();
            for index in 0..count {
                let entry_offset = start + index * entry_size;
                self.tick(entry_offset)?;
                let entry = self.read(entry_offset, entry_size)?;
                let tag = tiff.uint(&entry[..2]);
                if !matches!(tag, 700 | 330 | 34665 | 34853 | 40965) {
                    continue;
                }
                let kind = tiff.uint(&entry[2..4]);
                let n = tiff.uint(&entry[4..(entry_size - slot_size) as usize]);
                let size = match kind {
                    1 | 2 | 6 | 7 => 1,
                    3 | 8 => 2,
                    4 | 9 | 11 | 13 => 4,
                    5 | 10 | 12 | 16 | 17 | 18 => 8,
                    _ => {
                        self.record(malformed(
                            entry_offset,
                            "unsupported XMP/pointer TIFF field type",
                        ));
                        continue;
                    }
                };
                let length = n
                    .checked_mul(size)
                    .ok_or_else(|| malformed(entry_offset, "TIFF value size overflow"))?;
                let value_offset = if length <= slot_size {
                    entry_offset + entry_size - slot_size
                } else {
                    tiff.absolute(tiff.uint(&entry[(entry_size - slot_size) as usize..]))?
                };
                if tag == 700 {
                    self.simple(
                        Container::TiffTag700,
                        value_offset,
                        length,
                        format!("tiff:{base}:ifd:{offset}:entry:{index}", base = tiff.base),
                    )?;
                    self.packets.last_mut().expect("inserted packet").attributes = attrs(&[
                        ("ifd_offset", offset.to_string()),
                        ("type", kind.to_string()),
                        ("count", n.to_string()),
                    ]);
                    if !matches!(kind, 1 | 2 | 7) {
                        self.record(malformed(
                            entry_offset,
                            "XMP TIFF tag is not a byte/string type; raw bytes retained",
                        ));
                    }
                } else {
                    self.bounds(value_offset, length, tiff.end)?;
                    if !matches!(kind, 4 | 13 | 16 | 18) {
                        self.record(malformed(
                            entry_offset,
                            "invalid TIFF directory pointer type",
                        ));
                        continue;
                    }
                    let values = self.read(value_offset, length)?;
                    children.extend(values.chunks_exact(size as usize).map(|v| tiff.uint(v)));
                }
            }
            children.push(tiff.uint(&self.read(next_offset, slot_size)?));
            for child in children {
                if let Err(error) = self.tiff_ifd(tiff, child, depth + 1, active, visited) {
                    let stop = error.status == Status::ResourceLimit;
                    self.record(error);
                    if stop {
                        break;
                    }
                }
            }
            Ok(())
        })();
        active.remove(&offset);
        result
    }
}

#[derive(Clone)]
struct BmffBox {
    kind: [u8; 4],
    start: u64,
    payload: u64,
    end: u64,
}
#[derive(Default)]
struct Item {
    content_type: Vec<u8>,
    encoding: Vec<u8>,
    protection: u64,
    name: Vec<u8>,
}
struct Location {
    method: u64,
    reference: u64,
    base: u64,
    extents: Vec<(u64, u64)>,
}
const XMP_UUID: [u8; 16] = [
    0xbe, 0x7a, 0xcf, 0xcb, 0x97, 0xa9, 0x42, 0xe8, 0x9c, 0x71, 0x99, 0x94, 0x91, 0xe3, 0xaf, 0xac,
];
impl<R: Read + Seek> Parser<'_, R> {
    fn box_at(&mut self, offset: u64, end: u64) -> PResult<BmffBox> {
        self.tick(offset)?;
        self.bounds(offset, 8, end)?;
        let header = self.read(offset, 8)?;
        let size = be32(&header[..4]);
        let (size, header_length) = if size == 1 {
            self.bounds(offset, 16, end)?;
            let bytes = self.read(offset + 8, 8)?;
            (
                u64::from_be_bytes(bytes.try_into().expect("eight bytes")),
                16,
            )
        } else if size == 0 {
            (end - offset, 8)
        } else {
            (u64::from(size), 8)
        };
        if size < header_length {
            return Err(malformed(offset, "BMFF box smaller than its header"));
        }
        let next = self.bounds(offset, size, end)?;
        Ok(BmffBox {
            kind: header[4..8].try_into().expect("four bytes"),
            start: offset,
            payload: offset + header_length,
            end: next,
        })
    }
    fn boxes(&mut self, mut offset: u64, end: u64) -> PResult<Vec<BmffBox>> {
        let mut boxes = Vec::new();
        while offset < end {
            match self.box_at(offset, end) {
                Ok(entry) => {
                    offset = entry.end;
                    boxes.push(entry);
                }
                Err(error) => {
                    // A malformed later box cannot erase validated earlier carriers.
                    // Stop at the damaged boundary; never guess a resynchronization.
                    self.record(error);
                    break;
                }
            }
        }
        Ok(boxes)
    }
    fn bmff(&mut self, start: u64, end: u64, depth: usize) -> PResult<()> {
        if depth > self.limits.max_depth {
            return Err(limited(start, "BMFF nesting limit exceeded"));
        }
        for entry in self.boxes(start, end)? {
            match &entry.kind {
                b"uuid" => {
                    self.bounds(entry.payload, 16, entry.end)?;
                    if self.read(entry.payload, 16)? == XMP_UUID {
                        self.simple(
                            Container::BmffUuid,
                            entry.payload + 16,
                            entry.end - entry.payload - 16,
                            format!("bmff:uuid:{}", entry.start),
                        )?;
                    }
                }
                b"meta" => {
                    if let Err(error) = self.bmff_meta(&entry, depth + 1) {
                        self.record(error);
                    }
                }
                b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" | b"udta" => {
                    self.bmff(entry.payload, entry.end, depth + 1)?
                }
                _ => {}
            }
        }
        Ok(())
    }
    fn bmff_meta(&mut self, meta: &BmffBox, depth: usize) -> PResult<()> {
        if depth > self.limits.max_depth {
            return Err(limited(meta.start, "BMFF nesting limit exceeded"));
        }
        self.bounds(meta.payload, 4, meta.end)?;
        if self.read(meta.payload, 4)? != [0, 0, 0, 0] {
            return Err(unsupported(
                meta.payload,
                "unsupported BMFF meta version/flags",
            ));
        }
        let boxes = self.boxes(meta.payload + 4, meta.end)?;
        let mut items = BTreeMap::new();
        let mut locations = BTreeMap::new();
        let mut associations: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        let mut idat = None;
        for entry in &boxes {
            match &entry.kind {
                b"iinf" => self.bmff_iinf(entry, &mut items)?,
                b"iloc" => self.bmff_iloc(entry, &mut locations)?,
                b"iref" => self.bmff_iref(entry, &mut associations)?,
                b"idat" => {
                    if idat.replace(entry.clone()).is_some() {
                        return Err(malformed(entry.start, "duplicate BMFF idat"));
                    }
                }
                b"uuid" => {
                    self.bmff(entry.start, entry.end, depth + 1)?;
                }
                _ => {}
            }
        }
        for (id, item) in items {
            if item.content_type != b"application/rdf+xml" {
                continue;
            }
            let group = format!("bmff:meta:{}:item:{id}", meta.start);
            let Some(location) = locations.get(&id) else {
                self.record(malformed(meta.start, format!("XMP item {id} has no iloc")));
                continue;
            };
            if item.protection != 0 || location.reference != 0 || location.method > 1 {
                self.record(unsupported(meta.start, format!("XMP item {id} uses protection, external data reference, or unsupported construction method {}", location.method)));
                continue;
            }
            let (origin, limit) = if location.method == 1 {
                let Some(ref idat) = idat else {
                    self.record(malformed(meta.start, "XMP item references missing idat"));
                    continue;
                };
                (idat.payload, idat.end)
            } else {
                (0, self.length)
            };
            let result = (|| {
                let mut indices = Vec::new();
                let mut xml = Vec::new();
                for (ordinal, (offset, length)) in location.extents.iter().enumerate() {
                    // Zero-length extents mean the rest of the source in BMFF.
                    // libavif rejects them; guessing risks retaining image bytes.
                    if *length == 0 {
                        return Err(unsupported(
                            meta.start,
                            "zero-length XMP item extent is not supported",
                        ));
                    }
                    let offset = origin
                        .checked_add(location.base)
                        .and_then(|v| v.checked_add(*offset))
                        .ok_or_else(|| malformed(meta.start, "XMP item extent offset overflow"))?;
                    self.bounds(offset, *length, limit)?;
                    let bytes = self.read(offset, *length)?;
                    if bytes.len() > self.limits.max_packet_bytes.saturating_sub(xml.len()) {
                        return Err(limited(offset, "XMP item reconstruction limit exceeded"));
                    }
                    let attributes = attrs(&[
                        ("item_id", id.to_string()),
                        ("extent", ordinal.to_string()),
                        ("construction_method", location.method.to_string()),
                        (
                            "cdsc_to_item_ids",
                            associations
                                .get(&id)
                                .map(|ids| {
                                    ids.iter().map(u64::to_string).collect::<Vec<_>>().join(",")
                                })
                                .unwrap_or_default(),
                        ),
                        (
                            "content_type",
                            String::from_utf8_lossy(&item.content_type).into_owned(),
                        ),
                        (
                            "content_encoding",
                            String::from_utf8_lossy(&item.encoding).into_owned(),
                        ),
                        (
                            "item_name_hex",
                            item.name
                                .iter()
                                .map(|b| format!("{b:02x}"))
                                .collect::<String>(),
                        ),
                    ]);
                    let i = self.packet(
                        Container::BmffMime,
                        bytes.clone(),
                        vec![ByteRange {
                            offset,
                            length: *length,
                        }],
                        group.clone(),
                        attributes,
                    )?;
                    indices.push(i);
                    xml.extend_from_slice(&bytes);
                }
                if indices.is_empty() {
                    return Err(malformed(meta.start, "XMP item has no extents"));
                }
                let transform = if item.encoding.is_empty() {
                    Transformation::ExtentsConcatenated
                } else if item.encoding == b"gzip" {
                    let mut reader = flate2::read::MultiGzDecoder::new(xml.as_slice());
                    let mut output = Vec::new();
                    reader
                        .by_ref()
                        .take(self.limits.max_packet_bytes as u64 + 1)
                        .read_to_end(&mut output)
                        .map_err(|_| malformed(meta.start, "invalid gzip XMP item"))?;
                    if output.len() > self.limits.max_packet_bytes {
                        return Err(limited(meta.start, "decompressed XMP item limit exceeded"));
                    }
                    xml = output;
                    Transformation::GzipDecompressed
                } else {
                    return Err(unsupported(
                        meta.start,
                        "unsupported XMP item content encoding; source extents retained",
                    ));
                };
                self.input(xml, indices, transform, group)
            })();
            if let Err(error) = result {
                self.record(error);
            }
        }
        Ok(())
    }
    fn bmff_iinf(&mut self, entry: &BmffBox, items: &mut BTreeMap<u64, Item>) -> PResult<()> {
        self.bounds(entry.payload, 4, entry.end)?;
        let prefix = self.read(entry.payload, 4)?;
        let width = match prefix[0] {
            0 => 2,
            1 => 4,
            _ => return Err(unsupported(entry.start, "unsupported iinf version")),
        };
        self.bounds(entry.payload + 4, width, entry.end)?;
        let count_bytes = self.read(entry.payload + 4, width)?;
        let count = Slice::new(&count_bytes, entry.start).uint(width as usize)?;
        let children = self.boxes(entry.payload + 4 + width, entry.end)?;
        if children.len() as u64 != count {
            return Err(malformed(entry.start, "iinf entry count mismatch"));
        }
        for child in children {
            if &child.kind != b"infe" {
                return Err(malformed(child.start, "unexpected iinf child"));
            }
            let bytes = self.read(child.payload, child.end - child.payload)?;
            let mut cursor = Slice::new(&bytes, child.payload);
            let version = cursor.uint(1)?;
            cursor.take(3)?;
            let id = match version {
                2 => cursor.uint(2)?,
                3 => cursor.uint(4)?,
                0 | 1 => {
                    self.record(unsupported(
                        child.start,
                        "legacy infe version cannot establish MIME item coverage",
                    ));
                    continue;
                }
                _ => return Err(unsupported(child.start, "unsupported infe version")),
            };
            let protection = cursor.uint(2)?;
            let kind = cursor.take(4)?;
            let name = cursor.nul()?.to_vec();
            let mut item = Item {
                protection,
                name,
                ..Item::default()
            };
            if kind == b"mime" {
                item.content_type = cursor.nul()?.to_vec();
                item.encoding = if cursor.remaining().is_empty() {
                    vec![]
                } else {
                    cursor.nul()?.to_vec()
                };
            }
            if items.insert(id, item).is_some() {
                return Err(malformed(child.start, "duplicate BMFF item ID"));
            }
        }
        Ok(())
    }
    fn bmff_iloc(
        &mut self,
        entry: &BmffBox,
        locations: &mut BTreeMap<u64, Location>,
    ) -> PResult<()> {
        let bytes = self.read(entry.payload, entry.end - entry.payload)?;
        let mut cursor = Slice::new(&bytes, entry.payload);
        let version = cursor.uint(1)?;
        cursor.take(3)?;
        if version > 2 {
            return Err(unsupported(entry.start, "unsupported iloc version"));
        }
        let a = cursor.uint(1)?;
        let b = cursor.uint(1)?;
        let offset_size = (a >> 4) as usize;
        let length_size = (a & 15) as usize;
        let base_size = (b >> 4) as usize;
        let index_size = if version > 0 { (b & 15) as usize } else { 0 };
        if [offset_size, length_size, base_size, index_size]
            .iter()
            .any(|s| ![0, 4, 8].contains(s))
        {
            return Err(malformed(entry.start, "invalid iloc integer width"));
        }
        let count = cursor.uint(if version < 2 { 2 } else { 4 })?;
        if count > self.limits.max_entries as u64 {
            return Err(limited(entry.start, "iloc item count limit exceeded"));
        }
        for _ in 0..count {
            self.tick(entry.start)?;
            let id = cursor.uint(if version < 2 { 2 } else { 4 })?;
            let method = if version > 0 { cursor.uint(2)? & 15 } else { 0 };
            let reference = cursor.uint(2)?;
            let base = cursor.uint(base_size)?;
            let extent_count = cursor.uint(2)?;
            if extent_count > self.limits.max_entries as u64 {
                return Err(limited(entry.start, "iloc extent count limit exceeded"));
            }
            let mut extents = Vec::new();
            for _ in 0..extent_count {
                self.tick(entry.start)?;
                cursor.uint(index_size)?;
                extents.push((cursor.uint(offset_size)?, cursor.uint(length_size)?));
            }
            if locations
                .insert(
                    id,
                    Location {
                        method,
                        reference,
                        base,
                        extents,
                    },
                )
                .is_some()
            {
                return Err(malformed(entry.start, "duplicate iloc item ID"));
            }
        }
        if !cursor.remaining().is_empty() {
            return Err(malformed(entry.start, "trailing iloc fields"));
        }
        Ok(())
    }
    fn bmff_iref(
        &mut self,
        entry: &BmffBox,
        associations: &mut BTreeMap<u64, Vec<u64>>,
    ) -> PResult<()> {
        self.bounds(entry.payload, 4, entry.end)?;
        let prefix = self.read(entry.payload, 4)?;
        let width = match prefix[0] {
            0 => 2,
            1 => 4,
            _ => return Err(unsupported(entry.start, "unsupported iref version")),
        };
        for child in self.boxes(entry.payload + 4, entry.end)? {
            if &child.kind != b"cdsc" {
                continue;
            }
            let bytes = self.read(child.payload, child.end - child.payload)?;
            let mut cursor = Slice::new(&bytes, child.payload);
            let from = cursor.uint(width)?;
            let count = cursor.uint(2)?;
            for _ in 0..count {
                self.tick(child.start)?;
                associations
                    .entry(from)
                    .or_default()
                    .push(cursor.uint(width)?);
            }
            if !cursor.remaining().is_empty() {
                return Err(malformed(child.start, "trailing cdsc fields"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod bounded_hash_tests {
    use super::*;
    use std::io::Write;
    #[test]
    fn hashing_does_not_follow_growth_past_the_declared_revision() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"initial-appended-by-another-writer")
            .unwrap();
        let (hash, matches) = hash_file(&mut file, 7).unwrap();
        assert!(!matches);
        assert_eq!(hash, digest(b"initial"));
        assert_eq!(file.stream_position().unwrap(), 8);
        let (_, matches) = hash_file(&mut file, 100).unwrap();
        assert!(!matches);
    }

    #[cfg(unix)]
    #[test]
    fn nonregular_open_worker() {
        use std::os::unix::ffi::OsStrExt;
        let Some(mode) = std::env::var_os("PHOTOCATALOG_PACKET_OPEN_RACE") else {
            return;
        };
        let directory = std::path::PathBuf::from(
            std::env::var_os("PHOTOCATALOG_PACKET_RACE_DIRECTORY").unwrap(),
        );
        let path = directory.join("source");
        fs::write(&path, b"original").unwrap();
        // Keep the old inode alive so a replacement cannot reuse its identity.
        let _held_original = File::open(&path).unwrap();
        let expected = fs::symlink_metadata(&path).unwrap();
        fs::remove_file(&path).unwrap();
        match mode.to_str().unwrap() {
            "fifo" | "direct-fifo" => {
                let path_c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(path_c.as_ptr(), 0o600) }, 0);
            }
            "symlink" => {
                let target = directory.join("target");
                fs::write(&target, b"original").unwrap();
                std::os::unix::fs::symlink(target, &path).unwrap();
            }
            "regular" => {
                fs::write(&path, b"original").unwrap();
                File::options()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_times(fs::FileTimes::new().set_modified(expected.modified().unwrap()))
                    .unwrap();
            }
            _ => panic!("unknown race test mode"),
        }
        if mode == "direct-fifo" {
            assert_eq!(
                inspect(&path, &Limits::default()).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
            assert_eq!(
                inspect_sidecar(&path, &Limits::default())
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        } else {
            assert!(
                open_regular(&path, &expected).is_err(),
                "replacement accepted"
            );
        }
    }
}

#[cfg(test)]
mod stability_tests {
    use super::*;
    #[cfg(any(target_os = "macos", windows))]
    use std::io::Write;

    fn tiff(packet: bool) -> Vec<u8> {
        let xml = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'/>";
        let mut bytes = b"II*\0\x08\0\0\0".to_vec();
        bytes.extend(u16::from(packet).to_le_bytes());
        if packet {
            bytes.extend(700u16.to_le_bytes());
            bytes.extend(1u16.to_le_bytes());
            bytes.extend((xml.len() as u32).to_le_bytes());
            bytes.extend(26u32.to_le_bytes());
        }
        bytes.extend(0u32.to_le_bytes());
        if packet {
            bytes.extend(xml);
        }
        bytes.resize(8 * 1024 * 1024, 42);
        bytes
    }

    #[test]
    fn one_digest_and_bounded_metadata_reads_match_conservative_results() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("source");
        for (bytes, sidecar, expected) in [
            (tiff(true), false, Status::Complete),
            (tiff(false), false, Status::Absent),
            (vec![b'?'; 8 * 1024 * 1024], false, Status::Unsupported),
            (b"opaque sidecar\xff".to_vec(), true, Status::Complete),
        ] {
            fs::write(&path, &bytes).unwrap();
            let limits = Limits {
                max_metadata_read_bytes: 4096,
                ..Limits::default()
            };
            let (fast, work) =
                inspect_observed(&path, &limits, sidecar, false, &mut |_| Ok(())).unwrap();
            let (old, fallback) =
                inspect_observed(&path, &limits, sidecar, true, &mut |_| Ok(())).unwrap();
            assert_eq!(fast.status, expected);
            assert_eq!(
                serde_json::to_value(&fast).unwrap(),
                serde_json::to_value(&old).unwrap()
            );
            assert_eq!(fast.revision.blake3, digest(&bytes));
            let eligible = open_stable(&path, &fs::symlink_metadata(&path).unwrap(), false)
                .unwrap()
                .1;
            let passes = if eligible {
                1
            } else if cfg!(unix) {
                2
            } else {
                3
            };
            assert_eq!(work.whole_file_hash_passes, passes);
            assert_eq!(work.hash_bytes, bytes.len() as u64 * u64::from(passes));
            assert!(work.metadata_bytes <= 4096);
            assert_eq!(work.verification.starts_with("held_"), eligible);
            assert_eq!(
                fallback.whole_file_hash_passes,
                if cfg!(unix) { 2 } else { 3 }
            );
            assert_eq!(
                fallback.hash_bytes,
                fallback.whole_file_hash_passes as u64 * bytes.len() as u64
            );
            println!("packet work: {}", serde_json::to_string(&work).unwrap());
        }
        fs::write(&path, tiff(true)).unwrap();
        let limits = Limits {
            max_source_bytes: 1,
            ..Limits::default()
        };
        let (limited, work) = inspect_with_work(&path, &limits).unwrap();
        assert_eq!(limited.status, Status::ResourceLimit);
        assert!(limited.revision.blake3.is_empty());
        assert_eq!(work.hash_bytes, 0);
        assert_eq!(work.metadata_bytes, 0);
        let limits = Limits {
            max_metadata_read_bytes: 1,
            ..Limits::default()
        };
        let (limited, work) = inspect_with_work(&path, &limits).unwrap();
        assert_eq!(limited.status, Status::ResourceLimit);
        assert_eq!(
            work.whole_file_hash_passes,
            if open_stable(&path, &fs::symlink_metadata(&path).unwrap(), false)
                .unwrap()
                .1
            {
                1
            } else if cfg!(unix) {
                2
            } else {
                3
            }
        );
        assert_eq!(work.metadata_bytes, 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn restored_mtime_writes_and_transient_rewrites_reject_committed_revision() {
        for checkpoint in ["after_hash", "after_parse"] {
            for restore_bytes in [false, true] {
                let temp = tempfile::tempdir().unwrap();
                let path = temp.path().join("sidecar");
                let bytes = b"<old/>";
                fs::write(&path, bytes).unwrap();
                let held = File::open(&path).unwrap();
                assert!(
                    qualified_unix_file(&held),
                    "this precision regression requires local APFS"
                );
                let stamp = stability_stamp(&held).unwrap();
                let mut mutated = false;
                let (result, work) =
                    inspect_observed(&path, &Limits::default(), true, false, &mut |at| {
                        if at == checkpoint {
                            let mut writer = File::options().write(true).open(&path)?;
                            writer.write_all(b"<new/>")?;
                            if restore_bytes {
                                writer.seek(SeekFrom::Start(0))?;
                                writer.write_all(bytes)?;
                            }
                            writer.set_times(fs::FileTimes::new().set_modified(stamp.modified))?;
                            let current = stability_stamp(&held)?;
                            assert_eq!(current.modified, stamp.modified);
                            assert_eq!(current.bytes, stamp.bytes);
                            assert_ne!(
                                current.changed, stamp.changed,
                                "fixture must exercise actual ctime change"
                            );
                            mutated = true;
                        }
                        Ok(())
                    })
                    .unwrap();
                assert!(mutated);
                assert_eq!(result.status, Status::SourceChanged);
                assert!(
                    !result.packets.is_empty(),
                    "retain uncommitted packets with explicit changed status"
                );
                assert_eq!(work.whole_file_hash_passes, 1);
            }
        }
    }

    #[test]
    fn replaced_same_bytes_and_mtime_path_is_not_the_held_revision() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("source");
        fs::write(&path, b"<x/>").unwrap();
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        let (result, _) = inspect_observed(&path, &Limits::default(), true, false, &mut |at| {
            if at == "after_parse" {
                fs::rename(&path, temp.path().join("old-retained"))?;
                fs::write(&path, b"<x/>")?;
                File::options()
                    .write(true)
                    .open(&path)?
                    .set_times(fs::FileTimes::new().set_modified(mtime))?;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(result.status, Status::SourceChanged);
        assert_eq!(result.packets[0].bytes, b"<x/>");
    }

    #[cfg(windows)]
    #[test]
    fn windows_existing_writer_falls_back_and_held_lease_denies_new_writer() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("source");
        fs::write(&path, b"<old/>").unwrap();
        let mut writer = File::options().write(true).open(&path).unwrap();
        let (result, work) = inspect_observed(&path, &Limits::default(), true, false, &mut |at| {
            if at == "after_hash" {
                writer.write_all(b"<new/>")?;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(result.status, Status::SourceChanged);
        assert_eq!(work.verification, "conservative_full_rehash");
        assert!(work.whole_file_hash_passes >= 2);
        drop(writer);
        let (result, work) = inspect_observed(&path, &Limits::default(), true, false, &mut |_| {
            assert!(File::options().write(true).open(&path).is_err());
            Ok(())
        })
        .unwrap();
        assert_eq!(result.status, Status::Complete);
        assert_eq!(work.whole_file_hash_passes, 1);
        assert!(File::options().write(true).open(path).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn conservative_rehash_samples_path_after_final_hash() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("source");
        fs::write(&path, b"<x/>").unwrap();
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        let (result, work) = inspect_observed(&path, &Limits::default(), true, true, &mut |at| {
            if at == "after_fallback_hash" {
                fs::rename(&path, temp.path().join("old-retained"))?;
                fs::write(&path, b"<x/>")?;
                File::options()
                    .write(true)
                    .open(&path)?
                    .set_times(fs::FileTimes::new().set_modified(mtime))?;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(result.status, Status::SourceChanged);
        assert_eq!(work.whole_file_hash_passes, 2);
        assert_eq!(result.packets[0].bytes, b"<x/>");
    }

    #[cfg(windows)]
    #[test]
    fn windows_writable_mapping_survives_creator_and_forces_conservative_validation() {
        use std::ffi::c_void;
        use std::os::windows::io::AsRawHandle;
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
                    if !self.view.is_null() {
                        UnmapViewOfFile(self.view);
                    }
                    if !self.handle.is_null() {
                        CloseHandle(self.handle);
                    }
                }
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("mapped");
        fs::write(&path, b"<old/>").unwrap();
        let creator = File::options().read(true).write(true).open(&path).unwrap();
        let mut mapping = Mapping {
            handle: unsafe {
                CreateFileMappingW(
                    creator.as_raw_handle(),
                    std::ptr::null_mut(),
                    4,
                    0,
                    0,
                    std::ptr::null(),
                )
            },
            view: std::ptr::null_mut(),
        };
        assert!(!mapping.handle.is_null(), "{}", io::Error::last_os_error());
        mapping.view = unsafe { MapViewOfFile(mapping.handle, 2, 0, 0, 6) };
        assert!(!mapping.view.is_null(), "{}", io::Error::last_os_error());
        drop(creator);
        // A live writable mapping must not qualify as a held deny-write proof.
        // See CreateFileW's FILE_SHARE_WRITE contract linked at open_stable.
        let expected = fs::symlink_metadata(&path).unwrap();
        let (lease, eligible) = open_stable(&path, &expected, false).unwrap();
        assert!(
            !eligible,
            "writable mapping was incorrectly admitted as a write exclusion lease"
        );
        drop(lease);
        let (result, work) = inspect_observed(&path, &Limits::default(), true, false, &mut |at| {
            if at == "after_hash" {
                unsafe {
                    std::ptr::copy_nonoverlapping(b"<new/>".as_ptr(), mapping.view.cast::<u8>(), 6);
                }
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(result.status, Status::SourceChanged);
        assert_eq!(work.verification, "conservative_full_rehash");
        assert!(work.whole_file_hash_passes >= 2);
        drop(mapping);
        let (result, work) = inspect_sidecar_with_work(&path, &Limits::default()).unwrap();
        assert_eq!(result.status, Status::Complete);
        assert_eq!(work.whole_file_hash_passes, 1);
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn only_known_local_apfs_qualifies_for_timestamp_proof() {
        assert!(qualified_apfs_mount(b"apfs", true));
        for (name, local) in [
            (b"hfs".as_slice(), true),
            (b"exfat", true),
            (b"apfs", false),
            (b"", true),
            (b"apfs-extra", true),
        ] {
            assert!(!qualified_apfs_mount(name, local));
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn other_unix_files_use_conservative_hash_verification() {
        let file = tempfile::tempfile().unwrap();
        assert!(!qualified_unix_file(&file));
    }
}

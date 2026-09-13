//! JSON-only Source projection. Property order is retained evidence, not a
//! requirement to allocate Serde's generic Content tree for path unit arrays.
//! This does not replace NativePath's public serialization or interpretation.
use crate::{
    lightroom::{
        Issue, Limits,
        capture::{Artifact, Entry, Manifest, Request},
        source::Revision,
        wal::WalReport,
    },
    storage_volume::NativePath,
};
use anyhow::Result;
use serde::Deserialize;
use serde_json::value::RawValue;

#[derive(Deserialize)]
struct Path<'a> {
    #[serde(borrow)]
    encoding: &'a RawValue,
    #[serde(borrow)]
    units: &'a RawValue,
}
impl Path<'_> {
    fn decode(self) -> Result<NativePath> {
        // Both field orders reach a concrete numeric Vec deserializer directly.
        // Foreign units and NUL remain opaque evidence: no to_path or file I/O.
        // Delegate tag syntax to the original public decoder with an empty
        // unit list. The actual unit array never enters generic Content.
        // A malformed large tag can allocate its already admitted byte length
        // in this error-path probe; that temporary belongs in parser accounting.
        let probe = format!("{{\"encoding\":{},\"units\":[]}}", self.encoding.get());
        Ok(match serde_json::from_str::<NativePath>(&probe)? {
            NativePath::UnixBytes(_) => {
                NativePath::UnixBytes(serde_json::from_str(self.units.get())?)
            }
            NativePath::WindowsWide(_) => {
                NativePath::WindowsWide(serde_json::from_str(self.units.get())?)
            }
        })
    }
}
#[derive(Deserialize)]
struct RequestJson<'a> {
    #[serde(borrow)]
    source: Path<'a>,
    #[serde(borrow)]
    output: Path<'a>,
    include_auxiliary: bool,
    closed_application_evidence: Option<String>,
    limits: Limits,
}
impl RequestJson<'_> {
    fn decode(self) -> Result<Request> {
        Ok(Request {
            source: self.source.decode()?,
            output: self.output.decode()?,
            include_auxiliary: self.include_auxiliary,
            closed_application_evidence: self.closed_application_evidence,
            limits: self.limits,
        })
    }
}
#[derive(Deserialize)]
struct ArtifactJson<'a> {
    #[serde(borrow)]
    source: Path<'a>,
    role: String,
    #[serde(borrow)]
    relative: Path<'a>,
    stored: String,
    revision: Revision,
    blake3: String,
}
impl ArtifactJson<'_> {
    fn decode(self) -> Result<Artifact> {
        Ok(Artifact {
            source: self.source.decode()?,
            role: self.role,
            relative: self.relative.decode()?,
            stored: self.stored,
            revision: self.revision,
            blake3: self.blake3,
        })
    }
}
#[derive(Deserialize)]
struct EntryJson<'a> {
    #[serde(borrow)]
    path: Path<'a>,
    role: String,
    #[serde(borrow)]
    relative: Path<'a>,
    directory: bool,
    modified_ns: Option<u128>,
    changed: String,
}
impl EntryJson<'_> {
    fn decode(self) -> Result<Entry> {
        Ok(Entry {
            path: self.path.decode()?,
            role: self.role,
            relative: self.relative.decode()?,
            directory: self.directory,
            modified_ns: self.modified_ns,
            changed: self.changed,
        })
    }
}
#[derive(Deserialize)]
struct ManifestJson<'a> {
    protocol: u32,
    #[serde(borrow)]
    request: RequestJson<'a>,
    state: String,
    raw_byte_retention: String,
    sqlite_consistency: String,
    application_consistency: String,
    cooperative_lock_protocol: String,
    #[serde(borrow)]
    artifacts: Vec<ArtifactJson<'a>>,
    #[serde(borrow)]
    companion_inventory: Vec<EntryJson<'a>>,
    #[serde(borrow)]
    absent_companions: Vec<Path<'a>>,
    issues: Vec<Issue>,
    wal: Option<WalReport>,
    logical_blake3: Option<String>,
    logical_revision: Option<Revision>,
    revision_id: Option<String>,
}

fn convert<T, U>(values: Vec<T>, mut decode: impl FnMut(T) -> Result<U>) -> Result<Vec<U>> {
    // The borrowed staging Vec and exact-length output Vec coexist until the
    // conversion drains. Account for both; do not claim in-place conversion.
    let mut output = Vec::with_capacity(values.len());
    for value in values {
        output.push(decode(value)?);
    }
    Ok(output)
}

/// Decode only the typed view of already byte-admitted JSON. Callers retain and
/// verify the original bytes/digest; no normalization becomes source authority.
/// Unknown fields, duplicate known fields and sequence forms follow the same
/// derived struct rules as the original Manifest/Request/Entry definitions.
pub(crate) fn decode(bytes: &[u8]) -> Result<Manifest> {
    let value: ManifestJson<'_> = serde_json::from_slice(bytes)?;
    Ok(Manifest {
        protocol: value.protocol,
        request: value.request.decode()?,
        state: value.state,
        raw_byte_retention: value.raw_byte_retention,
        sqlite_consistency: value.sqlite_consistency,
        application_consistency: value.application_consistency,
        cooperative_lock_protocol: value.cooperative_lock_protocol,
        artifacts: convert(value.artifacts, ArtifactJson::decode)?,
        companion_inventory: convert(value.companion_inventory, EntryJson::decode)?,
        absent_companions: convert(value.absent_companions, Path::decode)?,
        issues: value.issues,
        wal: value.wal,
        logical_blake3: value.logical_blake3,
        logical_revision: value.logical_revision,
        revision_id: value.revision_id,
    })
}

#[cfg(test)]
mod tests;

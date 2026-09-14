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
use serde::{
    Deserialize,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use std::{borrow::Cow, fmt};

mod admission;
use admission::Footprint;
pub(crate) use admission::{array, units, units_bounded};
use serde_json::value::RawValue;

#[derive(Clone, Copy, Deserialize)]
enum Encoding {
    UnixBytes,
    WindowsWide,
}
struct Path<'a> {
    encoding: &'a RawValue,
    units: &'a RawValue,
    sequence: bool,
}
impl<'de: 'a, 'a> Deserialize<'de> for Path<'a> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct PathVisitor;
        impl<'de> Visitor<'de> for PathVisitor {
            type Value = Path<'de>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("native path")
            }
            fn visit_seq<S: SeqAccess<'de>>(
                self,
                mut seq: S,
            ) -> std::result::Result<Self::Value, S::Error> {
                let encoding = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;
                let units = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                if seq.next_element::<de::IgnoredAny>()?.is_some() {
                    return Err(de::Error::invalid_length(3, &self));
                }
                Ok(Path {
                    encoding,
                    units,
                    sequence: true,
                })
            }
            fn visit_map<M: MapAccess<'de>>(
                self,
                mut map: M,
            ) -> std::result::Result<Self::Value, M::Error> {
                #[derive(Deserialize)]
                #[serde(field_identifier)]
                enum Key {
                    #[serde(rename = "encoding")]
                    Encoding,
                    #[serde(rename = "units")]
                    Units,
                    #[serde(other)]
                    Other,
                }
                let (mut encoding, mut units) = (None, None);
                while let Some(key) = map.next_key()? {
                    match key {
                        Key::Encoding => {
                            if encoding.is_some() {
                                return Err(de::Error::duplicate_field("encoding"));
                            }
                            encoding = Some(map.next_value()?);
                        }
                        Key::Units => {
                            if units.is_some() {
                                return Err(de::Error::duplicate_field("units"));
                            }
                            units = Some(map.next_value()?);
                        }
                        Key::Other => {
                            map.next_value::<de::IgnoredAny>()?;
                        }
                    }
                }
                Ok(Path {
                    encoding: encoding.ok_or_else(|| de::Error::missing_field("encoding"))?,
                    units: units.ok_or_else(|| de::Error::missing_field("units"))?,
                    sequence: false,
                })
            }
        }
        d.deserialize_struct("NativePath", &["encoding", "units"], PathVisitor)
    }
}
impl Path<'_> {
    fn encoding(&self) -> Result<Encoding> {
        if self.sequence {
            // Public adjacent-tag sequences use an identifier, not deserialize_enum.
            let tag: Cow<'_, str> = serde_json::from_str(self.encoding.get())?;
            Ok(match tag.as_ref() {
                "UnixBytes" => Encoding::UnixBytes,
                "WindowsWide" => Encoding::WindowsWide,
                _ => anyhow::bail!("unknown native path encoding"),
            })
        } else {
            Ok(serde_json::from_str(self.encoding.get())?)
        }
    }
    fn inspect(&self, footprint: &mut Footprint, stop: &dyn Fn() -> bool) -> Result<()> {
        match self.encoding()? {
            Encoding::UnixBytes => footprint.path(
                units_bounded::<u8>(
                    self.units,
                    stop,
                    None,
                    (crate::lightroom::MANIFEST_BYTES + 5) / 2,
                )?
                .0,
                1,
            ),
            Encoding::WindowsWide => footprint.path(
                units_bounded::<u16>(
                    self.units,
                    stop,
                    None,
                    (crate::lightroom::MANIFEST_BYTES + 5) / 2,
                )?
                .0,
                2,
            ),
        }
    }
    #[cfg(test)]
    fn decode(self) -> Result<NativePath> {
        self.build(&|| false)
    }
    fn build(self, stop: &dyn Fn() -> bool) -> Result<NativePath> {
        Ok(match self.encoding()? {
            Encoding::UnixBytes => {
                let n = units_bounded::<u8>(
                    self.units,
                    stop,
                    None,
                    (crate::lightroom::MANIFEST_BYTES + 5) / 2,
                )?
                .0;
                NativePath::UnixBytes(units(self.units, stop, Some(n))?.1)
            }
            Encoding::WindowsWide => {
                let n = units_bounded::<u16>(
                    self.units,
                    stop,
                    None,
                    (crate::lightroom::MANIFEST_BYTES + 5) / 2,
                )?
                .0;
                NativePath::WindowsWide(units(self.units, stop, Some(n))?.1)
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
    fn decode(self, stop: &dyn Fn() -> bool) -> Result<Request> {
        Ok(Request {
            source: self.source.build(stop)?,
            output: self.output.build(stop)?,
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
    fn decode(self, stop: &dyn Fn() -> bool) -> Result<Artifact> {
        Ok(Artifact {
            source: self.source.build(stop)?,
            role: self.role,
            relative: self.relative.build(stop)?,
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
    fn decode(self, stop: &dyn Fn() -> bool) -> Result<Entry> {
        Ok(Entry {
            path: self.path.build(stop)?,
            role: self.role,
            relative: self.relative.build(stop)?,
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
    artifacts: &'a RawValue,
    #[serde(borrow)]
    companion_inventory: &'a RawValue,
    #[serde(borrow)]
    absent_companions: &'a RawValue,
    #[serde(borrow)]
    issues: &'a RawValue,
    wal: Option<WalReport>,
    logical_blake3: Option<String>,
    logical_revision: Option<Revision>,
    revision_id: Option<String>,
}

/// Retained callers already bind these bytes to immutable source evidence.
pub(crate) fn decode(bytes: &[u8]) -> Result<Manifest> {
    decode_cancellable(bytes, &|| false)
}

pub(crate) fn decode_cancellable(bytes: &[u8], stop: &dyn Fn() -> bool) -> Result<Manifest> {
    decode_bounded(bytes, super::super::MANIFEST_BYTES, stop)
}

/// Reply bytes may expand through JSON normalization; admission is additionally
/// bounded by the proved minimum possible original representation, not by the
/// helper's claimed origin alone. No normalization replaces original authority.
pub(crate) fn decode_reply(bytes: &[u8], stop: &dyn Fn() -> bool) -> Result<Manifest> {
    decode_bounded(bytes, 97 * 1024 * 1024, stop)
}
fn decode_bounded(bytes: &[u8], encoded_cap: usize, stop: &dyn Fn() -> bool) -> Result<Manifest> {
    anyhow::ensure!(bytes.len() <= encoded_cap, "capture manifest byte limit");
    anyhow::ensure!(!stop(), "source decoding canceled");
    let value: ManifestJson<'_> = serde_json::from_slice(bytes)?;
    let mut footprint = Footprint::default();
    footprint.strings(&[
        &value.state,
        &value.raw_byte_retention,
        &value.sqlite_consistency,
        &value.application_consistency,
        &value.cooperative_lock_protocol,
        value.logical_blake3.as_deref().unwrap_or(""),
        value.revision_id.as_deref().unwrap_or(""),
        value
            .request
            .closed_application_evidence
            .as_deref()
            .unwrap_or(""),
    ])?;
    if let Some(revision) = &value.logical_revision {
        footprint.strings(&[&revision.object, &revision.changed])?;
    }
    value.request.source.inspect(&mut footprint, stop)?;
    value.request.output.inspect(&mut footprint, stop)?;
    let a = array(value.artifacts, stop, |raw| {
        let item: ArtifactJson<'_> = serde_json::from_str(raw.get())?;
        footprint.member(57)?;
        item.source.inspect(&mut footprint, stop)?;
        item.relative.inspect(&mut footprint, stop)?;
        footprint.strings(&[
            &item.role,
            &item.stored,
            &item.blake3,
            &item.revision.object,
            &item.revision.changed,
        ])
    })?;
    let e = array(value.companion_inventory, stop, |raw| {
        let item: EntryJson<'_> = serde_json::from_str(raw.get())?;
        footprint.member(49)?;
        item.path.inspect(&mut footprint, stop)?;
        item.relative.inspect(&mut footprint, stop)?;
        footprint.strings(&[&item.role, &item.changed])
    })?;
    let p = array(value.absent_companions, stop, |raw| {
        footprint.member(17)?;
        serde_json::from_str::<Path<'_>>(raw.get())?.inspect(&mut footprint, stop)
    })?;
    let i = array(value.issues, stop, |raw| {
        let item: Issue = serde_json::from_str(raw.get())?;
        footprint.member(11)?;
        footprint.strings(&[
            &item.code,
            item.source_id.as_deref().unwrap_or(""),
            &item.detail,
        ])
    })?;
    // Four nonempty arrays each omit their last separator. Retaining this +4
    // slack avoids rejecting a valid boundary representation by an off-by-four.
    footprint.check()?;
    fn collect<T>(
        raw: &RawValue,
        count: usize,
        stop: &dyn Fn() -> bool,
        mut decode: impl FnMut(&RawValue) -> Result<T>,
    ) -> Result<Vec<T>> {
        let mut out = Vec::new();
        out.try_reserve_exact(count)?;
        array(raw, stop, |item| {
            anyhow::ensure!(out.len() < count, "source member count changed");
            out.push(decode(item)?);
            Ok(())
        })?;
        anyhow::ensure!(out.len() == count, "source member count changed");
        Ok(out)
    }
    let artifacts = collect(value.artifacts, a, stop, |raw| {
        serde_json::from_str::<ArtifactJson<'_>>(raw.get())?.decode(stop)
    })?;
    let companion_inventory = collect(value.companion_inventory, e, stop, |raw| {
        serde_json::from_str::<EntryJson<'_>>(raw.get())?.decode(stop)
    })?;
    let absent_companions = collect(value.absent_companions, p, stop, |raw| {
        serde_json::from_str::<Path<'_>>(raw.get())?.build(stop)
    })?;
    let issues = collect(value.issues, i, stop, |raw| {
        Ok(serde_json::from_str(raw.get())?)
    })?;
    anyhow::ensure!(!stop(), "source decoding canceled");
    Ok(Manifest {
        protocol: value.protocol,
        request: value.request.decode(stop)?,
        state: value.state,
        raw_byte_retention: value.raw_byte_retention,
        sqlite_consistency: value.sqlite_consistency,
        application_consistency: value.application_consistency,
        cooperative_lock_protocol: value.cooperative_lock_protocol,
        artifacts,
        companion_inventory,
        absent_companions,
        issues,
        wal: value.wal,
        logical_blake3: value.logical_blake3,
        logical_revision: value.logical_revision,
        revision_id: value.revision_id,
    })
}

#[cfg(test)]
mod tests;

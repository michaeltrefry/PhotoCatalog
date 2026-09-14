//! Bounded Artifact authority view; no filesystem access or identity inference.
use super::{ArtifactDescriptor, ArtifactMapping, ArtifactRequest, DESCRIPTOR_LIMIT};
use crate::{
    lightroom::{
        capture::Artifact,
        migration_source::{FileIdentity, manifest_json::path_mode, record_json::size},
    },
    storage_volume::NativePath,
};
use anyhow::{Result, ensure};
use serde::Deserialize;
use serde_json::value::RawValue;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Mapping<'a> {
    #[serde(borrow)]
    root: &'a RawValue,
    #[serde(borrow)]
    relative: &'a RawValue,
    copy_identity: FileIdentity,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request<'a> {
    retained_capture_record: i64,
    member_index: usize,
    #[serde(borrow)]
    mapping: Mapping<'a>,
}
// Artifact's public struct permits unknown fields, unlike its authority wrappers.
#[derive(Deserialize)]
struct Member<'a> {
    #[serde(borrow)]
    source: &'a RawValue,
    role: String,
    #[serde(borrow)]
    relative: &'a RawValue,
    stored: String,
    revision: FileIdentity,
    blake3: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Descriptor<'a> {
    protocol: u32,
    #[serde(borrow)]
    request: Request<'a>,
    selected_input: String,
    capture_revision: String,
    manifest_blake3: String,
    #[serde(borrow)]
    artifact: Member<'a>,
}
fn native(raw: &RawValue, stop: &dyn Fn() -> bool, buffered: bool) -> Result<NativePath> {
    path_mode(raw, DESCRIPTOR_LIMIT.div_ceil(2), stop, buffered)
}
/// A canonical descriptor was already limited to 64 KiB before source opens.
/// Admit each repeated path before allocation; typed scalar strings are at most
/// input_limit transiently and their retained sum must fit that existing limit.
pub(crate) fn decode(
    bytes: &[u8],
    input_limit: usize,
    stop: &dyn Fn() -> bool,
) -> Result<ArtifactDescriptor> {
    decode_mode(bytes, input_limit, stop, false)
}
pub(crate) fn decode_mode(
    bytes: &[u8],
    input_limit: usize,
    stop: &dyn Fn() -> bool,
    buffered: bool,
) -> Result<ArtifactDescriptor> {
    ensure!(
        bytes.len() <= input_limit && !stop(),
        "artifact authority byte admission/canceled"
    );
    let d: Descriptor<'_> = serde_json::from_slice(bytes)?;
    let m = d.request.mapping;
    let a = d.artifact;
    let strings = [
        &d.selected_input,
        &d.capture_revision,
        &d.manifest_blake3,
        &m.copy_identity.object,
        &m.copy_identity.changed,
        &a.role,
        &a.stored,
        &a.revision.object,
        &a.revision.changed,
        &a.blake3,
    ];
    ensure!(
        strings
            .iter()
            .try_fold(0usize, |sum, s| sum.checked_add(s.len()))
            .is_some_and(|sum| sum <= DESCRIPTOR_LIMIT),
        "artifact descriptor string admission"
    );
    let out = ArtifactDescriptor {
        protocol: d.protocol,
        request: ArtifactRequest {
            retained_capture_record: d.request.retained_capture_record,
            member_index: d.request.member_index,
            mapping: ArtifactMapping {
                root: native(m.root, stop, buffered)?,
                relative: native(m.relative, stop, buffered)?,
                copy_identity: m.copy_identity,
            },
        },
        selected_input: d.selected_input,
        capture_revision: d.capture_revision,
        manifest_blake3: d.manifest_blake3,
        artifact: Artifact {
            source: native(a.source, stop, buffered)?,
            role: a.role,
            relative: native(a.relative, stop, buffered)?,
            stored: a.stored,
            revision: a.revision,
            blake3: a.blake3,
        },
    };
    size(&out, DESCRIPTOR_LIMIT)?;
    ensure!(!stop(), "artifact authority canceled");
    Ok(out)
}
#[cfg(test)]
mod tests;

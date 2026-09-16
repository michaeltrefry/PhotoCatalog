//! Source-only InputSeal projection. Public serde grammar and exact original
//! bytes remain authoritative; repeated rosters are admitted before allocation.
use super::manifest_json::{array, path_mode};
use super::{FileIdentity, InputSeal, SelectedCapture, SelectionApproval, SupplementPin};
use anyhow::{Result, ensure};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::value::RawValue;

fn empty() -> &'static RawValue {
    // Fixed literal, no caller text and no owned graph.
    serde_json::from_str("[]").expect("literal empty array")
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Seal<'a> {
    protocol: u32,
    #[serde(borrow)]
    database: &'a RawValue,
    identity: FileIdentity,
    blake3: String,
    approval: SelectionApproval,
    #[serde(borrow)]
    selected: &'a RawValue,
    #[serde(borrow)]
    excluded_revisions: &'a RawValue,
    #[serde(borrow, default = "empty")]
    supplements: &'a RawValue,
}

fn add(total: &mut usize, bytes: usize, maximum: usize) -> Result<()> {
    *total = total
        .checked_add(bytes)
        .ok_or_else(|| anyhow::anyhow!("seal capacity overflow"))?;
    ensure!(*total <= maximum, "seal decoded string/unit admission");
    Ok(())
}
fn strings(total: &mut usize, values: &[&str], maximum: usize) -> Result<()> {
    for value in values {
        add(total, value.len(), maximum)?;
    }
    Ok(())
}
/// Inspect every typed member without retaining a span/member vector; then
/// reserve exactly the admitted count. The one current typed member is dropped
/// before the output vector is allocated. Direct typed u128/enum semantics stay.
pub(crate) fn roster<T: DeserializeOwned>(
    raw: &RawValue,
    maximum: usize,
    stop: &dyn Fn() -> bool,
    mut inspect: impl FnMut(&T) -> Result<()>,
) -> Result<Vec<T>> {
    let mut count = 0usize;
    array(raw, stop, |item| {
        ensure!(count < maximum, "source opening roster admission");
        let member: T = serde_json::from_str(item.get())?;
        inspect(&member)?;
        count += 1;
        Ok(())
    })?;
    let mut out = Vec::new();
    out.try_reserve_exact(count)?;
    array(raw, stop, |item| {
        ensure!(out.len() < count, "source opening roster count changed");
        out.push(serde_json::from_str(item.get())?);
        Ok(())
    })?;
    Ok(out)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Pin<'a> {
    revision: String,
    source_id: String,
    origin: String,
    source_revision: crate::xmp_packets::SourceRevision,
    #[serde(borrow)]
    historical_status: &'a RawValue,
    proof_blake3: String,
}
fn pin(raw: &RawValue, buffered: bool) -> Result<SupplementPin> {
    let v: Pin<'_> = serde_json::from_str(raw.get())?;
    Ok(SupplementPin {
        revision: v.revision,
        source_id: v.source_id,
        origin: v.origin,
        source_revision: v.source_revision,
        historical_status: super::buffered_json::unit_enum(v.historical_status, buffered)?,
        proof_blake3: v.proof_blake3,
    })
}
/// `input_limit` is the caller's existing raw-byte limit (retention 8 MiB,
/// artifact seal 16 MiB, or complete Source authority 32 MiB). All accepted seals
/// already require a canonical binding no larger than MANIFEST_BYTES. This
/// supplies a conservative decoded-string/unit floor, not a new public ceiling.
pub(crate) fn decode(
    bytes: &[u8],
    input_limit: usize,
    stop: &dyn Fn() -> bool,
) -> Result<InputSeal> {
    decode_mode(bytes, input_limit, stop, false)
}
pub(crate) fn decode_mode(
    bytes: &[u8],
    input_limit: usize,
    stop: &dyn Fn() -> bool,
    buffered: bool,
) -> Result<InputSeal> {
    ensure!(
        bytes.len() <= input_limit && !stop(),
        "seal byte admission/canceled"
    );
    let s: Seal<'_> = serde_json::from_slice(bytes)?;
    let maximum = crate::lightroom::MANIFEST_BYTES;
    let mut owned = 0usize;
    strings(
        &mut owned,
        &[
            &s.identity.object,
            &s.identity.changed,
            &s.blake3,
            &s.approval.document_blake3,
            &s.approval.scope,
            &s.approval.roster_blake3,
        ],
        maximum,
    )?;
    let database = path_mode(s.database, maximum.div_ceil(2), stop, buffered)?;
    let n = match &database {
        crate::storage_volume::NativePath::UnixBytes(v) => v.len(),
        crate::storage_volume::NativePath::WindowsWide(v) => v.len(),
    };
    add(
        &mut owned,
        n.checked_mul(2)
            .ok_or_else(|| anyhow::anyhow!("seal units overflow"))?
            .saturating_sub(1),
        maximum,
    )?;
    let selected = roster::<SelectedCapture>(s.selected, 16_384, stop, |v| {
        strings(
            &mut owned,
            &[
                &v.revision,
                &v.family,
                &v.family_evidence_digest,
                &v.manifest_blake3,
            ],
            maximum,
        )
    })?;
    let excluded_revisions =
        roster::<String>(s.excluded_revisions, 16_384 - selected.len(), stop, |v| {
            strings(&mut owned, &[v], maximum)
        })?;
    let mut count = 0usize;
    array(s.supplements, stop, |raw| {
        ensure!(count < 16_384, "source opening roster admission");
        let v = pin(raw, buffered)?;
        strings(
            &mut owned,
            &[
                &v.revision,
                &v.source_id,
                &v.origin,
                &v.source_revision.blake3,
                &v.proof_blake3,
            ],
            maximum,
        )?;
        count += 1;
        Ok(())
    })?;
    let mut supplements = Vec::new();
    supplements.try_reserve_exact(count)?;
    array(s.supplements, stop, |raw| {
        ensure!(
            supplements.len() < count,
            "source opening roster count changed"
        );
        supplements.push(pin(raw, buffered)?);
        Ok(())
    })?;
    ensure!(!stop(), "seal decoding canceled");
    Ok(InputSeal {
        protocol: s.protocol,
        database,
        identity: s.identity,
        blake3: s.blake3,
        approval: s.approval,
        selected,
        excluded_revisions,
        supplements,
    })
}

#[cfg(test)]
mod tests;

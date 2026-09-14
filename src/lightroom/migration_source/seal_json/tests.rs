use super::*;
use crate::{
    storage_volume::NativePath,
    xmp_packets::{SourceRevision, Status},
};
use std::cell::Cell;
fn sample() -> InputSeal {
    InputSeal {
        protocol: 1,
        database: NativePath::WindowsWide(vec![55296, 0, 65535]),
        identity: FileIdentity {
            object: "object".into(),
            bytes: u64::MAX,
            modified_ns: Some(u128::MAX),
            changed: "changed".into(),
        },
        blake3: "b".repeat(64),
        approval: SelectionApproval {
            document_blake3: "a".repeat(64),
            scope: "selected_migration_test".into(),
            roster_blake3: "c".repeat(64),
        },
        selected: vec![SelectedCapture {
            revision: "d".repeat(64),
            family: "family".into(),
            family_evidence_digest: "e".repeat(64),
            manifest_blake3: "f".repeat(64),
            evidence_revision: i64::MAX,
        }],
        excluded_revisions: vec![],
        supplements: vec![SupplementPin {
            revision: "d".repeat(64),
            source_id: "source".into(),
            origin: "embedded".into(),
            source_revision: SourceRevision {
                length: u64::MAX,
                blake3: "a".repeat(64),
                modified_unix_ns: Some(u128::MAX),
            },
            historical_status: Status::SourceChanged,
            proof_blake3: "b".repeat(64),
        }],
    }
}
fn parity(raw: &str, positive: bool) -> Result<()> {
    let old = serde_json::from_str::<InputSeal>(raw);
    let new = decode(raw.as_bytes(), crate::lightroom::MANIFEST_BYTES, &|| false);
    assert_eq!(old.is_ok(), positive, "public outcome: {raw}");
    assert_eq!(new.is_ok(), positive, "private outcome: {raw}");
    if let (Ok(old), Ok(new)) = (old, new) {
        assert_eq!(serde_json::to_vec(&old)?, serde_json::to_vec(&new)?);
        assert_eq!(old.binding_blake3()?, new.binding_blake3()?);
    }
    Ok(())
}
#[test]
fn seal_all_fields_statuses_sequences_defaults_and_scalar_parity() -> Result<()> {
    for status in [
        Status::Absent,
        Status::Complete,
        Status::Unsupported,
        Status::Malformed,
        Status::ResourceLimit,
        Status::SourceChanged,
    ] {
        let mut s = sample();
        s.supplements[0].historical_status = status;
        let raw = serde_json::to_string(&s)?;
        parity(&raw, true)?;
        let tag = serde_json::to_string(&status)?;
        parity(
            &raw.replace(
                &format!("\"historical_status\":{tag}"),
                &format!("\"historical_status\":{{{tag}:null}}"),
            ),
            true,
        )?;
    }
    let s = sample();
    let seq = serde_json::to_string(&(
        s.protocol,
        &s.database,
        &s.identity,
        &s.blake3,
        &s.approval,
        &s.selected,
        &s.excluded_revisions,
        &s.supplements,
    ))?;
    parity(&seq, true)?;
    let shortened = serde_json::to_string(&(
        s.protocol,
        &s.database,
        &s.identity,
        &s.blake3,
        &s.approval,
        &s.selected,
        &s.excluded_revisions,
    ))?;
    parity(&shortened, true)?;
    let raw = serde_json::to_string(&s)?;
    let no_supplements = raw[..raw.find(",\"supplements\":").unwrap()].to_owned() + "}";
    parity(&no_supplements, true)?;
    parity(
        &(no_supplements[..no_supplements.len() - 1].to_owned() + ",\"supplements\":null}"),
        false,
    )?;
    parity(&raw.replacen("{", "{\"protocol\":1,", 1), false)?;
    parity(&raw.replacen("{", "{\"unknown\":0,", 1), false)?;
    parity(
        &raw.replacen("{", "{\"$serde_json::private::RawValue\":\"{}\",", 1),
        false,
    )?;
    parity(
        &raw.replace(
            &u128::MAX.to_string(),
            "340282366920938463463374607431768211456",
        ),
        false,
    )?;
    // Direct typed ignored fields retain IgnoredAny semantics, not Value's
    // overflow-number rejection or RawValue first-key interpretation.
    parity(
        &raw.replacen("\"object\":", "\"ignored\":1e9999,\"object\":", 1),
        true,
    )?;
    parity(
        &raw.replacen(
            "\"encoding\":\"WindowsWide\",\"units\":[55296,0,65535]",
            "\"units\":[55296,0,65535],\"encoding\":\"WindowsWide\"",
            1,
        ),
        true,
    )?;
    Ok(())
}
#[test]
fn seal_rosters_precede_reservation_and_keep_exact_capacities() -> Result<()> {
    let mut s = sample();
    s.identity.modified_ns = None;
    s.supplements.clear();
    let mut raw = serde_json::to_value(&s)?;
    // Value is used only by this fixture after replacing full-u128 fields.
    raw["identity"]["modified_ns"] = serde_json::Value::Null;
    raw["supplements"] = serde_json::json!([]);
    raw["selected"] = serde_json::json!([]);
    raw["excluded_revisions"] = serde_json::Value::Array(vec![serde_json::json!("é"); 16_384]);
    let bytes = serde_json::to_vec(&raw)?;
    let out = decode(&bytes, crate::lightroom::MANIFEST_BYTES, &|| false)?;
    assert_eq!(out.excluded_revisions.len(), 16_384);
    assert_eq!(out.excluded_revisions.capacity(), 16_384);
    assert!(
        matches!(&out.database,NativePath::WindowsWide(v) if v.len()==v.capacity()&&v.as_slice()==[55296,0,65535])
    );
    println!(
        "C seal sizes: InputSeal={} SelectedCapture={} SupplementPin={} String={}",
        std::mem::size_of::<InputSeal>(),
        std::mem::size_of::<SelectedCapture>(),
        std::mem::size_of::<SupplementPin>(),
        std::mem::size_of::<String>()
    );
    let visits = Cell::new(0usize);
    let body = "[".to_owned() + &vec!["0"; 4097].join(",") + "]";
    let body: &RawValue = serde_json::from_str(&body)?;
    assert!(
        roster::<u64>(body, 4096, &|| false, |_| {
            visits.set(visits.get() + 1);
            Ok(())
        })
        .is_err()
    );
    assert_eq!(visits.get(), 4096);
    assert!(decode(&bytes, crate::lightroom::MANIFEST_BYTES, &|| true).is_err());
    let checks = Cell::new(0usize);
    assert!(
        decode(&bytes, crate::lightroom::MANIFEST_BYTES, &|| {
            checks.set(checks.get() + 1);
            checks.get() > 20
        })
        .is_err()
    );
    Ok(())
}
#[test]
fn seal_callers_keep_distinct_raw_byte_limits() -> Result<()> {
    let raw = serde_json::to_vec(&sample())?;
    let mut padded = raw.clone();
    padded.resize(8 * 1024 * 1024 + 1, b' ');
    assert!(decode(&padded, 8 * 1024 * 1024, &|| false).is_err());
    let out = decode(&padded, 16 * 1024 * 1024, &|| false)?;
    assert_eq!(serde_json::to_vec(&out)?, raw);
    Ok(())
}

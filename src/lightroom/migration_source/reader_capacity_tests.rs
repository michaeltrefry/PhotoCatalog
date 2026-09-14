use super::*;
use crate::{
    capacity_probes as probe,
    lightroom::migration_source::{seal_json, tests::Fixture},
};

#[test]
fn capacity_seal_sets_and_opening_keep_caller_limits() -> Result<()> {
    let fixture = Fixture::new();
    let mut seal = fixture.seal.clone();
    assert_eq!(seal.excluded_revisions.len(), 1);
    // This synthetic maximum fills every combined selected/excluded slot.
    // Keep the physical fixture unchanged for the later real opening.
    seal.excluded_revisions.clear();
    let template = seal.selected[0].clone();
    seal.selected = (0..16_384)
        .map(|i| {
            let mut selected = template.clone();
            selected.revision = format!("{i:064x}");
            selected.family = format!("family-{i:05}");
            selected
        })
        .collect();
    seal.supplements = (0..16_384)
        .map(|i| super::super::SupplementPin {
            revision: format!("{i:064x}"),
            source_id: format!("{i:0128x}"),
            origin: "embedded".into(),
            source_revision: crate::xmp_packets::SourceRevision {
                length: 1,
                blake3: "a".repeat(64),
                modified_unix_ns: None,
            },
            historical_status: crate::xmp_packets::Status::Complete,
            proof_blake3: "b".repeat(64),
        })
        .collect();
    seal.approval.roster_blake3 = seal.roster_blake3()?;
    let raw = serde_json::to_vec(&seal)?;
    assert!(raw.len() > 8 * 1024 * 1024 && raw.len() <= 16 * 1024 * 1024);
    let baseline = probe::begin();
    assert!(seal_json::decode(&raw, 8 * 1024 * 1024, &|| false).is_err());
    let decoded = seal_json::decode(&raw, 16 * 1024 * 1024, &|| false)?;
    decoded.validate()?;
    assert_eq!(decoded.selected.len(), 16_384);
    assert_eq!(decoded.supplements.len(), 16_384);
    // Also exercise the exact combined boundary with both roster arms nonempty.
    let mut mixed = decoded.clone();
    let excluded = mixed.selected.pop().unwrap();
    mixed.excluded_revisions.push(excluded.revision);
    mixed.approval.roster_blake3 = mixed.roster_blake3()?;
    let mixed_raw = serde_json::to_vec(&mixed)?;
    let mixed_decoded = seal_json::decode(&mixed_raw, 16 * 1024 * 1024, &|| false)?;
    mixed_decoded.validate()?;
    assert_eq!(mixed_decoded.selected.len(), 16_383);
    assert_eq!(mixed_decoded.excluded_revisions.len(), 1);
    println!("CAPACITY_SEAL_MIXED selected=16383 excluded=1 combined=16384 accepted=true");
    drop((mixed, mixed_raw, mixed_decoded));
    assert!(probe::visits(probe::SEAL_SETS) > 0);
    println!(
        "CAPACITY_SEAL raw={} raw_capacity={} decoded_owned={} selected_capacity={} supplement_capacity={}",
        raw.len(),
        raw.capacity(),
        probe::seal(&decoded),
        decoded.selected.capacity(),
        decoded.supplements.capacity()
    );
    let mut small = decoded.clone();
    small.supplements.clear();
    let smaller = serde_json::to_vec(&small)?;
    assert!(smaller.len() <= 8 * 1024 * 1024);
    seal_json::decode(&smaller, 8 * 1024 * 1024, &|| false)?.validate()?;
    small.selected.push(template);
    let oversized = serde_json::to_vec(&small)?;
    assert!(seal_json::decode(&oversized, 16 * 1024 * 1024, &|| false).is_err());
    drop((small, decoded, seal, raw, smaller, oversized));
    grammar_boundaries(&fixture.seal)?;
    // A separate small valid physical fixture exercises the real source opening
    // phase. The large selection above is a grammar/validation case, not a fake DB.
    let source = fixture.open();
    source.verify()?;
    drop(source);
    probe::report("seal-opening", baseline);
    Ok(())
}

// These are valid public typed grammar documents, not filesystem authorities.
// No path conversion/open is performed for the zero-unit/foreign path cases.
fn grammar_boundaries(template: &InputSeal) -> Result<()> {
    use crate::storage_volume::NativePath;
    for maximum in [8 * 1024 * 1024, 16 * 1024 * 1024] {
        let mut expected = template.clone();
        expected.identity.changed.clear();
        expected.database = NativePath::UnixBytes(vec![0]);
        let empty_bytes = serde_json::to_vec(&expected)?.len();
        // All relevant escaping forms plus literal multibyte UTF-8. The unit's
        // serialized size is measured, not assumed from character count.
        let unit = "\0\"\\é";
        let encoded_unit = serde_json::to_vec(unit)?.len() - 2;
        let count = (maximum - empty_bytes) / encoded_unit;
        expected.identity.changed = unit.repeat(count);
        let remainder = maximum - empty_bytes - count * encoded_unit;
        expected.identity.changed.push_str(&"x".repeat(remainder));
        let mut raw = serde_json::to_vec(&expected)?;
        assert_eq!(raw.len(), maximum);
        let before = blake3::hash(&raw);
        let decoded = seal_json::decode(&raw, maximum, &|| false)?;
        assert_eq!(decoded.identity.changed, expected.identity.changed);
        assert_eq!(decoded.database, expected.database);
        assert_eq!(blake3::hash(&raw), before);
        println!(
            "CAPACITY_SEAL_ESCAPED limit={maximum} raw={} raw_capacity={} decoded_string_len={} decoded_string_capacity={} exact_bytes=true",
            raw.len(),
            raw.capacity(),
            decoded.identity.changed.len(),
            decoded.identity.changed.capacity()
        );
        raw.push(b' '); // Still valid JSON; the existing caller byte bound rejects.
        assert!(seal_json::decode(&raw, maximum, &|| false).is_err());
        drop((decoded, raw, expected));
        for wide in [false, true] {
            let mut expected = template.clone();
            expected.identity.changed.clear();
            expected.database = if wide {
                NativePath::WindowsWide(vec![])
            } else {
                NativePath::UnixBytes(vec![])
            };
            let empty_bytes = serde_json::to_vec(&expected)?.len();
            // A nonempty zero-unit array contributes exactly 2*n-1 bytes over
            // its empty array. This is the largest n for this typed document
            // at the caller raw limit; it is not a global path-semantics claim.
            let units = (maximum - empty_bytes + 1) / 2;
            expected.database = if wide {
                NativePath::WindowsWide(vec![0; units])
            } else {
                NativePath::UnixBytes(vec![0; units])
            };
            let mut raw = serde_json::to_vec(&expected)?;
            assert!(raw.len() <= maximum && maximum - raw.len() < 2);
            let digest = blake3::hash(&raw);
            let decoded = seal_json::decode(&raw, maximum, &|| false)?;
            assert_eq!(decoded.database, expected.database);
            assert_eq!(
                probe::path(&decoded.database),
                units * if wide { 2 } else { 1 }
            );
            assert_eq!(blake3::hash(&raw), digest);
            println!(
                "CAPACITY_SEAL_PATH limit={maximum} wide={wide} raw={} raw_capacity={} units={units} decoded_path_capacity={} exact_bytes=true",
                raw.len(),
                raw.capacity(),
                probe::path(&decoded.database)
            );
            // Add one real unit to the serialized array via the public typed
            // oracle, proving its complete valid JSON cannot fit this cap.
            match &mut expected.database {
                NativePath::UnixBytes(v) => v.push(0),
                NativePath::WindowsWide(v) => v.push(0),
            }
            drop(decoded);
            raw = serde_json::to_vec(&expected)?;
            assert!(raw.len() > maximum);
            assert!(seal_json::decode(&raw, maximum, &|| false).is_err());
        }
    }
    Ok(())
}

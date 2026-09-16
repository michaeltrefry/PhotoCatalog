use super::*;
use std::cell::Cell;

const REVISION: &str = r#"{"length":7,"blake3":"retained","modified_unix_ns":42}"#;

fn row(revision: &str, status: &str) -> String {
    format!(r#"{{"origin":"embedded","revision":{revision},"status":{status}}}"#)
}
fn baseline(row: &str) -> String {
    format!(r#"{{"inspections":[{row}]}}"#)
}
fn original(bytes: &[u8]) -> Result<(SourceRevision, Status)> {
    let evidence: Value = serde_json::from_slice(bytes)?;
    let matches = evidence
        .get("inspections")
        .and_then(Value::as_array)
        .context("supplement has no retained original inspection")?
        .iter()
        .filter(|v| v.get("origin").and_then(Value::as_str) == Some("embedded"))
        .collect::<Vec<_>>();
    ensure!(
        matches.len() == 1,
        "supplement original association missing or ambiguous"
    );
    Ok((
        SourceRevision::deserialize(
            matches[0]
                .get("revision")
                .context("supplement source revision missing")?,
        )?,
        Status::deserialize(
            matches[0]
                .get("status")
                .context("supplement status missing")?,
        )?,
    ))
}
fn parity(raw: &str) {
    let before = blake3::hash(raw.as_bytes());
    let old = original(raw.as_bytes());
    let new = select(raw.as_bytes(), "embedded", &|| false);
    assert_eq!(old.is_ok(), new.is_ok(), "{raw}: {old:?} / {new:?}");
    if let (Ok(old), Ok(new)) = (old, new) {
        assert_eq!(old, new, "{raw}");
    }
    assert_eq!(before, blake3::hash(raw.as_bytes()));
}

#[test]
fn complete_value_number_duplicate_sequence_and_error_parity() {
    for revision in [
        REVISION,
        r#"[7,"retained",42]"#,
        r#"[7,"retained"]"#,
        r#"[7,"retained",null]"#,
        r#"[7,"retained",42,0]"#,
        r#"[7,"retained",42,0,1,2]"#,
        r#"{"length":{},"length":7,"blake3":"first","blake3":"last"}"#,
        r#"{"length":18446744073709551615,"blake3":"r","modified_unix_ns":18446744073709551615}"#,
        r#"{"length":18446744073709551616,"blake3":"r"}"#,
        r#"{"length":7,"blake3":"r","modified_unix_ns":340282366920938463463374607431768211455}"#,
        r#"{"length":7.0,"blake3":"r"}"#,
        r#"{"length":-1,"blake3":"r"}"#,
        r#"{"length":7,"blake3":[],"modified_unix_ns":{}}"#,
        r#"{"length":7,"blake3":"r","unknown":{"deep":[true,false,null,1e2]}}"#,
        r#"{"length":7,"blake3":"r","unknown":[1e400]}"#,
        r#"{"length":7,"blake3":"r","unknown":-1e400}"#,
        "null",
        "[]",
        "{}",
        "7",
        "true",
        r#""revision""#,
    ] {
        for status in [
            r#""Absent""#,
            r#""Complete""#,
            r#""Unsupported""#,
            r#""Malformed""#,
            r#""ResourceLimit""#,
            r#""SourceChanged""#,
            r#"{"Complete":null}"#,
            r#"{"Complete":0,"Complete":null}"#,
            r#"{"Complete":null,"Complete":{}}"#,
            r#"{"Complete":null,"Absent":null,"Other":null}"#,
            r#""unknown""#,
            "null",
            "[]",
            "0",
        ] {
            parity(&baseline(&row(revision, status)));
        }
    }
    let valid = row(REVISION, r#""Complete""#);
    for raw in [
        format!(r#"{{"inspections":null,"inspections":[{valid}]}}"#),
        format!(r#"{{"inspections":[{valid}],"inspections":null}}"#),
        format!(r#"{{"inspections":[{valid},{valid}]}}"#),
        format!(r#"{{"inspections":[{{"origin":"other"}},{valid}]}}"#),
        baseline(&format!(
            r#"{{"origin":"other","origin":"embedded","revision":{REVISION},"status":null,"status":"Complete"}}"#
        )),
        baseline(&format!(
            r#"{{"origin":"embedded","revision":{{"length":[],"blake3":0}},"revision":{REVISION},"status":{{"unknown":[0]}},"status":"Complete"}}"#
        )),
        format!(
            r#"{{"inspections":[{{"origin":"embedded","revision":false,"status":[]}}],"inspections":[{valid}]}}"#
        ),
        format!(r#"{{"inspections":[{valid}],"ignored":[1e400]}}"#),
        format!(r#"{{"ignored":[1e400],"ignored":0,"inspections":[{valid}]}}"#),
        format!(
            r#"{{"inspections":[{valid}],"unknown":{{"__proto__":{{"constructor":[null]}}}}}}"#
        ),
        format!("{} {}", baseline(&valid), "null"),
        "null".into(),
        "[]".into(),
        "{}".into(),
        "{\"inspections\":{}}".into(),
    ] {
        parity(&raw);
    }
    for depth in [16, 125, 126, 127, 128, 129, 150] {
        let raw = format!(
            r#"{{"inspections":[{valid}],"ignored":{}0{}}}"#,
            "[".repeat(depth),
            "]".repeat(depth)
        );
        parity(&raw);
    }
}

fn wrap(raw: &str) -> String {
    format!(
        r#"{{"{RAW_VALUE}":{}}}"#,
        serde_json::to_string(raw).unwrap()
    )
}

#[test]
fn first_key_raw_value_semantics_remain_at_every_projected_and_ignored_level() {
    let valid = row(REVISION, r#""Complete""#);
    let mut raw = baseline(&valid);
    for _ in 0..6 {
        raw = wrap(&raw);
        parity(&raw);
    }
    for revision in [
        wrap(REVISION),
        wrap(&wrap(REVISION)),
        wrap("1e400"),
        wrap("null"),
        wrap("{} null"),
    ] {
        parity(&baseline(&row(&revision, &wrap(r#""Complete""#))));
    }
    parity(&format!(
        r#"{{"inspections":{}}}"#,
        wrap(&format!("[{valid}]"))
    ));
    parity(&baseline(&wrap(&valid)));
    parity(&baseline(&format!(
        r#"{{"origin":{},"revision":{REVISION},"status":{}}}"#,
        wrap(r#""embedded""#),
        wrap(r#"{"Complete":null}"#)
    )));
    for ignored in [
        wrap("1e400"),
        wrap("[0,1]"),
        wrap("{invalid}"),
        wrap(&wrap("true")),
        format!(r#"{{"{RAW_VALUE}":0}}"#),
    ] {
        parity(&format!(
            r#"{{"inspections":[{valid}],"ignored":{ignored}}}"#
        ));
    }
    // Reserved keys after the first position remain ordinary object members.
    parity(&format!(
        r#"{{"inspections":[{valid}],"{RAW_VALUE}":"invalid"}}"#
    ));
    parity(&format!(
        r#"{{"{RAW_VALUE}":{},"later":0}}"#,
        serde_json::to_string(&baseline(&valid)).unwrap()
    ));
    parity(&format!(
        r#"{{"$serde_json::private::Raw\u0056alue":{}}}"#,
        serde_json::to_string(&baseline(&valid)).unwrap()
    ));
}

fn nodes(value: &Value) -> usize {
    1 + match value {
        Value::Array(values) => values.iter().map(nodes).sum(),
        Value::Object(values) => values.values().map(nodes).sum(),
        _ => 0,
    }
}

#[test]
fn full_baseline_retains_constant_projection_and_cancels_inside_unknown_trees() -> Result<()> {
    let valid = row(REVISION, r#""Complete""#);
    let prefix = format!(r#"{{"inspections":[{valid}],"unknown": ["#);
    let entry = r#"{"arbitrary":[0,null,true]},"#;
    let count = (crate::lightroom::PAGE_BYTES - prefix.len() - 3) / entry.len();
    let mut raw = prefix;
    for _ in 0..count {
        raw.push_str(entry);
    }
    raw.push_str("0]}");
    assert!(raw.len() <= crate::lightroom::PAGE_BYTES);
    assert!(raw.len() > crate::lightroom::PAGE_BYTES - entry.len());
    let before = blake3::hash(raw.as_bytes());
    let view = project(raw.as_bytes(), "embedded", &|| false)?;
    assert_eq!(
        nodes(&view),
        nodes(&project(baseline(&valid).as_bytes(), "embedded", &|| {
            false
        })?)
    );
    assert_eq!(
        select(raw.as_bytes(), "embedded", &|| false)?,
        original(baseline(&valid).as_bytes())?
    );
    let checks = Cell::new(0usize);
    let stop = || {
        let n = checks.get() + 1;
        checks.set(n);
        n > 1000
    };
    assert!(
        format!(
            "{:#}",
            select(raw.as_bytes(), "embedded", &stop).unwrap_err()
        )
        .contains("canceled")
    );
    assert!(checks.get() <= 1002);
    assert!(select(raw.as_bytes(), "embedded", &|| true).is_err());
    assert_eq!(before, blake3::hash(raw.as_bytes()));
    let over = vec![b' '; crate::lightroom::PAGE_BYTES + 1];
    assert!(
        format!("{:#}", select(&over, "embedded", &|| false).unwrap_err())
            .contains("metadata limit")
    );
    eprintln!(
        "supplement baseline bytes={} unknown_members={} retained_projection_nodes={}",
        raw.len(),
        count,
        nodes(&view)
    );
    Ok(())
}

#[test]
fn actual_reader_preserves_reserved_and_duplicate_evidence_bytes_and_identity() -> Result<()> {
    use crate::lightroom::migration_source::{
        MigrationSource, ReadLimits, SupplementPin, tests::Fixture,
    };
    let mut fixture = Fixture::new();
    let revision = fixture.revision().to_owned();
    let original_revision = REVISION.replace("retained", &"c".repeat(64));
    let raw = wrap(&format!(
        r#"{{"inspections":null,"inspections":[{}],"unused":{{"nested":[0,true,null]}}}}"#,
        row(&original_revision, r#""Malformed""#)
    ));
    fixture.edit(|db| {
        db.execute(
            "UPDATE paths SET state='available_packet_gaps',evidence=?1 WHERE revision=?2",
            rusqlite::params![raw, revision],
        )
        .unwrap();
    });
    let (source_revision, historical_status) = original(raw.as_bytes())?;
    fixture.seal.supplements.push(SupplementPin {
        revision,
        source_id: "file-selected".into(),
        origin: "embedded".into(),
        source_revision,
        historical_status,
        proof_blake3: "e".repeat(64),
    });
    let before = std::fs::read(&fixture.path)?;
    drop(fixture.open());
    assert_eq!(before, std::fs::read(&fixture.path)?);
    fixture.seal.supplements[0].source_revision.length += 1;
    assert!(MigrationSource::open(fixture.seal, ReadLimits::default()).is_err());
    assert_eq!(before, std::fs::read(&fixture.path)?);
    Ok(())
}

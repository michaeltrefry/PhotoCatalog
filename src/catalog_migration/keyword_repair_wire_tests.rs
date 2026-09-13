//! Independent compact-JSON/BLAKE3 vectors shared with the Python observer.
use super::*;
use serde::de::DeserializeOwned;
fn same<T: DeserializeOwned + Serialize>(v: &serde_json::Value) -> Result<T> {
    let value: T = serde_json::from_value(v["value"].clone())?;
    let bytes = encode(&value)?;
    assert_eq!(bytes, v["json"].as_str().context("golden JSON")?.as_bytes());
    assert_eq!(digest(&bytes), v["blake3"]);
    Ok(value)
}
#[test]
fn keyword_repair_wire_golden_matches_python_receipt_roster_and_state() -> Result<()> {
    let v: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/keyword_repair_wire.json"
    ))?;
    assert_eq!(v["protocol"], 1);
    let old: Receipt = same(&v["receipt"])?;
    let binding: Binding = same(&v["binding"])?;
    let _: Progress = same(&v["progress"])?;
    let _: Vec<(String, String, String, String)> = same(&v["new_receipts"])?;
    let mut prior = digest(ADAPTER.as_bytes());
    for (index, stage) in [KEYWORDS, MEMBERS].into_iter().enumerate() {
        let item = &v["roster"][index];
        let expected: (String, String, i64, String, Option<String>) = same(item)?;
        assert_eq!(expected.0, prior);
        assert_eq!(expected.1, stage);
        let archive = Archive {
            origin: binding.request.roots[0].origin.clone(),
            outcome: item["old_outcome"]
                .as_str()
                .context("golden outcome")?
                .as_bytes()
                .to_vec(),
            receipt: if item["with_receipt"] == true {
                Some(old.clone())
            } else {
                None
            },
            decision: None,
        };
        let actual = roster_next(&prior, stage, expected.2, &archive)?;
        assert_eq!(actual, item["blake3"]);
        prior = actual;
    }
    assert_eq!(prior, binding.request.expected_roster_blake3);
    Ok(())
}

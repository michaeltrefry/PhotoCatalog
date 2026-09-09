use anyhow::{Result, ensure};
use std::{fs, path::Path, process::Command};
fn run(root: &Path, name: &str, args: &[&str]) -> Result<(bool, serde_json::Value)> {
    let output = root.join(format!("{name}.json"));
    let result = Command::new(env!("CARGO_BIN_EXE_organization_probe"))
        .arg("--catalog")
        .arg(root.join("catalog"))
        .arg("--output")
        .arg(&output)
        .args(args)
        .output()?;
    let receipt = serde_json::from_slice(&fs::read(output)?)?;
    Ok((result.status.success(), receipt))
}
#[test]
fn all_frozen_queries_prove_small_fixture_and_corrupt_rows_fail_closed() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path();
    ensure!(run(root, "prepare", &["prepare", "--count", "1000"])?.0);
    let before = blake3::hash(&fs::read(root.join("catalog/catalog.sqlite3"))?);
    for case in [
        "browse",
        "rating",
        "rating-sort",
        "capture-rating",
        "filename-reverse",
        "keyword",
        "wide-keyword",
        "collection",
        "mixed",
        "text",
        "text-capture",
        "camera-lens",
        "date-camera",
        "label-flag",
        "folder",
        "folder-recursive",
        "conflicted",
    ] {
        let (okay, receipt) = run(
            root,
            case,
            &["query", case, "--repetitions", "2", "--warmups", "0"],
        )?;
        ensure!(okay && receipt["complete"] == true, "{case}: {receipt}");
        let samples = receipt["samples"].as_array().unwrap();
        ensure!(samples.len() == 2);
        for sample in samples {
            ensure!(
                sample["rows"].as_array().unwrap().len()
                    == sample["oracle_sequences"].as_array().unwrap().len()
            );
            ensure!(
                sample["chunks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|c| c["sorts"] == 0)
            );
        }
    }
    ensure!(before == blake3::hash(&fs::read(root.join("catalog/catalog.sqlite3"))?));
    let copy = root.join("mutation-copy");
    fs::create_dir(&copy)?;
    let copy_catalog = copy.join("catalog");
    fs::create_dir(&copy_catalog)?;
    fs::copy(
        root.join("catalog/catalog.sqlite3"),
        copy_catalog.join("catalog.sqlite3"),
    )?;
    let (okay, transition) = run(&copy, "transition", &["transitions", "--repetitions", "2"])?;
    ensure!(
        okay && transition["complete"] == true,
        "transition errors: {}",
        transition["errors"]
    );
    ensure!(
        transition["writes"].as_array().unwrap().len() == 2
            && transition["source_updates"].as_array().unwrap().len() == 2
    );
    ensure!(transition["source_hashes_before"] == transition["source_hashes_after"]);
    ensure!(before == blake3::hash(&fs::read(root.join("catalog/catalog.sqlite3"))?));
    let db = rusqlite::Connection::open(root.join("catalog/catalog.sqlite3"))?;
    db.execute(
        "UPDATE organization_assets SET label='corrupted' WHERE sequence IN(501,546)",
        [],
    )?;
    drop(db);
    let (okay, receipt) = run(
        root,
        "corrupt",
        &["query", "browse", "--repetitions", "2", "--warmups", "0"],
    )?;
    ensure!(
        !okay && receipt["complete"] == false && receipt["errors"].as_array().unwrap().len() == 2
    );
    ensure!(
        receipt["samples"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["rows"].as_array().unwrap().len() == 200 && s["error"].as_str().is_some())
    );
    let retained = fs::read(root.join("corrupt.json"))?;
    let result = Command::new(env!("CARGO_BIN_EXE_organization_probe"))
        .arg("--catalog")
        .arg(root.join("catalog"))
        .arg("--output")
        .arg(root.join("corrupt.json"))
        .args(["query", "browse", "--repetitions", "1"])
        .output()?;
    ensure!(!result.status.success() && fs::read(root.join("corrupt.json"))? == retained);
    Ok(())
}

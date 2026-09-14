use super::*;
use crate::{
    catalog_migration::importer::{KeywordOverlap, OverlapPolicy},
    lightroom::migration_source::{FileIdentity, SelectedCapture, SelectionApproval},
};
fn root() -> NativePath {
    NativePath::from_path(std::path::Path::new(if cfg!(windows) {
        r"C:\synthetic-destination"
    } else {
        "/synthetic-destination"
    }))
}
fn fixture() -> Result<Documents> {
    let policy = Policy {
        import_source: "independent-source-images".into(),
        overlap: OverlapPolicy::RequireDecision,
        keyword_overlap: KeywordOverlap::RequireDecision,
        artifacts: vec![],
        supplements: vec![],
    };
    let approval = ApprovalDocument {
        protocol: 1,
        review_token: "a".repeat(64),
        scope: ApprovalScope::SelectedMigrationTest,
        destination: root(),
        policy: policy.clone(),
        supplements: vec![],
        authorization: "Explicit synthetic test only".into(),
    };
    let approval_json = format!("  {}\n", serde_json::to_string(&approval)?);
    let mut seal = InputSeal {
        protocol: 1,
        database: root(),
        identity: FileIdentity {
            object: "synthetic-no-open".into(),
            bytes: 4096,
            modified_ns: None,
            changed: "fixture".into(),
        },
        blake3: "b".repeat(64),
        approval: SelectionApproval {
            document_blake3: blake3::hash(approval_json.as_bytes()).to_hex().to_string(),
            scope: "selected_migration_test".into(),
            roster_blake3: String::new(),
        },
        selected: vec![SelectedCapture {
            revision: "c".repeat(64),
            family: "family".into(),
            family_evidence_digest: "d".repeat(64),
            manifest_blake3: "e".repeat(64),
            evidence_revision: 1,
        }],
        excluded_revisions: vec![],
        supplements: vec![],
    };
    seal.approval.roster_blake3 = seal.roster_blake3()?;
    Ok(Documents {
        seal_json: serde_json::to_string(&seal)?,
        approval_json,
        policy_json: serde_json::to_string(&policy)?,
        execution_authorization_json: None,
    })
}
fn parse(d: Documents) -> Result<ApprovedDocuments> {
    let h = blake3::hash(d.approval_json.as_bytes())
        .to_hex()
        .to_string();
    ApprovedDocuments::parse(d, root(), &h)
}
#[test]
fn exact_approval_bytes_destination_policy_and_test_scope_bind_admission() -> Result<()> {
    let d = fixture()?;
    let old = d.approval_json.clone();
    let a = parse(d.clone())?;
    assert_eq!(a.documents().approval_json, old);
    assert_eq!(a.token(), parse(d.clone())?.token());
    assert_eq!(
        a.token(),
        blake3::hash(&serde_json::to_vec(&(
            "desktop-lightroom-execution-v1",
            &d,
            &root()
        ))?)
        .to_hex()
        .as_str()
    );
    let mut changed = d.clone();
    changed.policy_json = changed
        .policy_json
        .replace("RequireDecision", "ReuseExactPath");
    assert!(parse(changed).is_err());
    let mut changed = d.clone();
    changed.policy_json = changed
        .policy_json
        .replace("independent-source-images", "different-owner");
    assert!(parse(changed).is_err());
    let mut changed = d.clone();
    let mut seal: InputSeal = serde_json::from_str(&changed.seal_json)?;
    seal.approval.scope = "selected_migration".into();
    changed.seal_json = serde_json::to_string(&seal)?;
    assert!(parse(changed).is_err());
    let expected = blake3::hash(old.as_bytes()).to_hex().to_string();
    let mut changed = d.clone();
    changed.approval_json.push(' ');
    assert!(ApprovedDocuments::parse(changed, root(), &expected).is_err());
    let mut different = root().to_path()?;
    different.push("another");
    assert!(ApprovedDocuments::parse(d, NativePath::from_path(&different), &expected).is_err());
    Ok(())
}
#[test]
fn opaque_history_requires_separate_pinned_authorization_without_rewriting() -> Result<()> {
    let mut d = fixture()?;
    d.approval_json = "Original unstructured CLI approval: exact bytes\n".into();
    let mut seal: InputSeal = serde_json::from_str(&d.seal_json)?;
    seal.approval.document_blake3 = blake3::hash(d.approval_json.as_bytes())
        .to_hex()
        .to_string();
    d.seal_json = serde_json::to_string(&seal)?;
    assert!(parse(d.clone()).is_err());
    let policy: Policy = serde_json::from_str(&d.policy_json)?;
    let a = ExecutionAuthorization {
        protocol: 1,
        approval_blake3: seal.approval.document_blake3.clone(),
        source_binding: seal.binding_blake3()?,
        policy_blake3: blake3::hash(&serde_json::to_vec(&policy)?)
            .to_hex()
            .to_string(),
        destination: root(),
        scope: ApprovalScope::SelectedMigrationTest,
        authorization: "Explicitly resume pinned historical test".into(),
    };
    d.execution_authorization_json = Some(serde_json::to_string(&a)?);
    let bytes = d.approval_json.clone();
    assert_eq!(parse(d.clone())?.documents().approval_json, bytes);
    let mut stale = a;
    stale.source_binding = "f".repeat(64);
    d.execution_authorization_json = Some(serde_json::to_string(&stale)?);
    assert!(parse(d).is_err());
    Ok(())
}
#[test]
fn structured_authority_cannot_downgrade_after_tampering() -> Result<()> {
    let mut d = fixture()?;
    d.approval_json = d.approval_json.replace("\"protocol\":1", "\"protocol\":2");
    let mut seal: InputSeal = serde_json::from_str(&d.seal_json)?;
    seal.approval.document_blake3 = blake3::hash(d.approval_json.as_bytes())
        .to_hex()
        .to_string();
    d.seal_json = serde_json::to_string(&seal)?;
    assert!(parse(d).is_err());
    Ok(())
}
#[test]
fn foreign_local_destination_and_overbudget_documents_are_rejected_lexically() -> Result<()> {
    let foreign = if cfg!(windows) {
        NativePath::UnixBytes(b"/foreign".to_vec())
    } else {
        NativePath::WindowsWide(r"C:\foreign".encode_utf16().collect())
    };
    assert!(local_destination(&foreign).is_err());
    let mut d = fixture()?;
    d.approval_json = "x".repeat(DOCUMENT_BYTES + 1);
    assert!(parse(d).is_err());
    Ok(())
}

#[test]
fn lm_executor_batch3_borrowed_approved_documents_preserve_raw_identity_and_semantics() -> Result<()>
{
    let documents = fixture()?;
    let expected = blake3::hash(documents.approval_json.as_bytes())
        .to_hex()
        .to_string();
    let owned = ApprovedDocuments::parse(documents.clone(), root(), &expected)?;
    let admitted = ApprovedAdmitted::parse(
        &documents.seal_json,
        &documents.approval_json,
        &documents.policy_json,
        documents.execution_authorization_json.as_deref(),
        root(),
        &expected,
    )?;
    assert_eq!(admitted.token(), owned.token());
    assert_eq!(admitted.policy_blake3(), owned.policy_blake3());
    assert_eq!(
        serde_json::to_vec(admitted.seal())?,
        serde_json::to_vec(owned.seal())?
    );
    assert_eq!(
        serde_json::to_vec(admitted.policy())?,
        serde_json::to_vec(owned.policy())?
    );
    assert_eq!(
        admitted.exact_documents(),
        (
            documents.seal_json.as_str(),
            documents.approval_json.as_str(),
            documents.policy_json.as_str(),
            documents.execution_authorization_json.as_deref(),
        )
    );
    assert_eq!(
        admitted.approval_bytes(),
        documents.approval_json.as_bytes()
    );
    Ok(())
}

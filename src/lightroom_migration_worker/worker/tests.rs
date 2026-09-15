use super::*;
use crate::lightroom_migration_worker::{process, protocol::InputRole};
use std::path::Path;

#[cfg(unix)]
pub(super) mod unix;

fn descriptor(role: InputRole, text: &str) -> PartDescriptor {
    PartDescriptor {
        role,
        bytes: U64(text.len() as u64),
        blake3: blake3::hash(text.as_bytes()).to_hex().to_string(),
    }
}

fn base(operation: Operation, parts: Vec<PartDescriptor>, destination: &Path) -> Envelope {
    Envelope {
        protocol: 1,
        build: build_identity().into(),
        target_token: "b".repeat(64),
        destination: NativePath::from_path(destination),
        expected_destination: None,
        protected: vec![],
        parts,
        operation,
    }
}

#[test]
fn lm_executor_batch3_seven_operations_have_exact_document_rosters_and_derived_results()
-> Result<()> {
    let root = tempfile::tempdir()?;
    let destination = root.path().join("synthetic-destination");
    let path = destination.as_path();
    let run = Operation::Run {
        approval_blake3: "a".repeat(64),
        max_steps: U64(1),
        max_seconds: U64(1),
        source_open_ms: U64(1),
        artifact_open_ms: U64(1),
        max_artifact_bytes: U64(1),
    };
    let repair = || Operation::RepairCurrent {
        max_steps: U64(1),
        max_seconds: U64(1),
        source_open_ms: U64(1),
    };
    let keywords = || Operation::RepairKeywords {
        max_steps: U64(1),
        max_seconds: U64(1),
        source_open_ms: U64(1),
    };
    let cases = [
        (
            run,
            vec![InputRole::Seal, InputRole::Approval, InputRole::Policy],
        ),
        (
            Operation::Status {
                run: "a".repeat(64),
            },
            vec![],
        ),
        (
            Operation::PrepareSupplements,
            vec![InputRole::SupplementRequests],
        ),
        (
            repair(),
            vec![
                InputRole::Seal,
                InputRole::Approval,
                InputRole::RepairRequest,
            ],
        ),
        (
            Operation::RepairStatus {
                repair: "a".repeat(64),
            },
            vec![],
        ),
        (
            keywords(),
            vec![
                InputRole::Seal,
                InputRole::Approval,
                InputRole::RepairRequest,
            ],
        ),
        (
            Operation::KeywordRepairStatus {
                repair: "a".repeat(64),
            },
            vec![],
        ),
    ];
    for (operation, roles) in cases {
        let parts = roles
            .into_iter()
            .map(|role| descriptor(role, "x"))
            .collect();
        let envelope = base(operation, parts, path);
        envelope.validate()?;
        assert!(envelope.operation.result_maximum()? >= 2);
        let mut repeated = envelope.clone();
        repeated.parts.push(descriptor(InputRole::Approval, "x"));
        assert!(repeated.validate().is_err());
    }
    let mut authorization = base(
        Operation::Run {
            approval_blake3: "a".repeat(64),
            max_steps: U64(1),
            max_seconds: U64(1),
            source_open_ms: U64(1),
            artifact_open_ms: U64(1),
            max_artifact_bytes: U64(1),
        },
        [
            InputRole::Seal,
            InputRole::Approval,
            InputRole::Policy,
            InputRole::ExecutionAuthorization,
        ]
        .into_iter()
        .map(|role| descriptor(role, "x"))
        .collect(),
        path,
    );
    authorization.validate()?;
    authorization.parts.swap(2, 3);
    assert!(authorization.validate().is_err());
    Ok(())
}

#[test]
fn lm_executor_batch3_build_and_role_mismatch_refuse_before_filesystem_use() -> Result<()> {
    let root = tempfile::tempdir()?;
    let absent = root.path().join("must-remain-absent");
    let missing = absent.as_path();
    let mut envelope = base(
        Operation::Status {
            run: "a".repeat(64),
        },
        vec![],
        missing,
    );
    envelope.build = "c".repeat(64);
    let error = envelope.validate().unwrap_err();
    assert!(format!("{error:#}").contains("build mismatch"));
    assert!(!missing.exists());
    assert_eq!(
        process::Role::LightroomMigration.argument(),
        "--lightroom-migration-worker"
    );
    assert_eq!(
        process::Role::SourceSql.argument(),
        "--lightroom-source-reader-sql"
    );
    assert_eq!(
        process::Role::SourceRaw.argument(),
        "--lightroom-source-reader-raw"
    );
    assert_eq!(
        process::Role::SourceCaptureSql.argument(),
        "--lightroom-source-reader-capture-sql"
    );
    Ok(())
}

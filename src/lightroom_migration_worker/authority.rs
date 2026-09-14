//! Exact approval bytes remain immutable. Parsed descriptors bind actual
//! execution inputs; they never replace historical approval or source bytes.
use crate::{
    catalog_migration::importer::Policy,
    lightroom::{
        migration_source::{InputSeal, MigrationSource, ReadLimits},
        selection::{ApprovalDocument, ApprovalScope},
    },
    storage_volume::NativePath,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    path::Component,
    sync::{Arc, atomic::AtomicBool},
};

pub const DOCUMENT_BYTES: usize = 8 * 1024 * 1024;
pub const BUNDLE_BYTES: usize = 4 * DOCUMENT_BYTES;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Documents {
    pub seal_json: String,
    pub approval_json: String,
    pub policy_json: String,
    pub execution_authorization_json: Option<String>,
}
/// A separate explicit desktop decision for an already existing opaque CLI
/// approval. Its bytes do not modify or become the historical seal's approval.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionAuthorization {
    pub protocol: u32,
    pub approval_blake3: String,
    pub source_binding: String,
    pub policy_blake3: String,
    pub destination: NativePath,
    pub scope: ApprovalScope,
    pub authorization: String,
}

fn hash(s: &str) -> Result<()> {
    ensure!(
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "migration digest must be lowercase BLAKE3"
    );
    Ok(())
}
fn reason(s: &str) -> Result<()> {
    ensure!(
        !s.trim().is_empty() && s.len() <= 4096 && !s.contains('\0'),
        "explicit bounded execution authorization required"
    );
    Ok(())
}
pub fn local_destination(path: &NativePath) -> Result<std::path::PathBuf> {
    let units = match path {
        NativePath::UnixBytes(v) => v.len(),
        NativePath::WindowsWide(v) => v.len(),
    };
    ensure!(
        (1..=32768).contains(&units),
        "migration destination native-unit limit"
    );
    let p = path.to_path()?;
    ensure!(
        p.is_absolute()
            && !p
                .components()
                .any(|c| matches!(c, Component::CurDir | Component::ParentDir)),
        "migration destination must be an absolute lexical directory path"
    );
    Ok(p)
}
fn scope(s: ApprovalScope) -> &'static str {
    match s {
        ApprovalScope::SelectedMigration => "selected_migration",
        ApprovalScope::SelectedMigrationTest => "selected_migration_test",
    }
}
fn serialized_hash(value: &impl Serialize) -> Result<String> {
    let mut digest = blake3::Hasher::new();
    serde_json::to_writer(&mut digest, value)?;
    Ok(digest.finalize().to_hex().to_string())
}
fn same<T: Serialize>(a: &T, b: &T) -> Result<bool> {
    Ok(serialized_hash(a)? == serialized_hash(b)?)
}

/// Pure document admission. Filesystem/SQLite identity and the full immutable
/// source seal still require open_source on the isolated helper before writes.
pub struct ApprovedDocuments {
    documents: Documents,
    seal: InputSeal,
    policy: Policy,
    destination: NativePath,
    policy_blake3: String,
    token: String,
}
impl ApprovedDocuments {
    pub fn parse(
        documents: Documents,
        destination: NativePath,
        expected_approval_blake3: &str,
    ) -> Result<Self> {
        local_destination(&destination)?;
        let total = [
            &documents.seal_json,
            &documents.approval_json,
            &documents.policy_json,
        ]
        .into_iter()
        .chain(documents.execution_authorization_json.iter())
        .try_fold(0usize, |sum, raw| {
            ensure!(
                !raw.is_empty() && raw.len() <= DOCUMENT_BYTES,
                "migration document byte limit"
            );
            sum.checked_add(raw.len())
                .context("migration document aggregate overflow")
        })?;
        ensure!(
            total <= BUNDLE_BYTES,
            "migration document aggregate byte limit"
        );
        hash(expected_approval_blake3)?;
        let actual = blake3::hash(documents.approval_json.as_bytes())
            .to_hex()
            .to_string();
        ensure!(
            actual == expected_approval_blake3,
            "explicit approval bytes changed"
        );
        let seal: InputSeal =
            serde_json::from_str(&documents.seal_json).context("parse selected input seal")?;
        ensure!(
            seal.approval.document_blake3 == actual,
            "seal approval digest differs"
        );
        let policy: Policy =
            serde_json::from_str(&documents.policy_json).context("parse exact import policy")?;
        let policy_blake3 = serialized_hash(&policy)?;
        // A document that claims the structured approval shape must validate as
        // that shape. Unknown fields/type errors cannot downgrade to legacy.
        let structured = serde_json::from_str::<serde_json::Value>(&documents.approval_json)
            .ok()
            .is_some_and(|v| {
                v.as_object().is_some_and(|o| {
                    o.contains_key("review_token")
                        || o.contains_key("destination")
                        || o.contains_key("policy")
                })
            });
        if structured {
            let approval: ApprovalDocument = serde_json::from_str(&documents.approval_json)
                .context("structured selection approval invalid; legacy fallback refused")?;
            ensure!(
                approval.protocol == 1,
                "unsupported selection approval protocol"
            );
            hash(&approval.review_token)?;
            reason(&approval.authorization)?;
            ensure!(
                same(&approval.destination, &destination)?,
                "approval destination differs from actual root"
            );
            ensure!(
                same(&approval.policy, &policy)?,
                "approval policy differs from execution policy"
            );
            ensure!(
                scope(approval.scope) == seal.approval.scope,
                "approval scope differs from sealed scope"
            );
            ensure!(
                same(&approval.supplements, &seal.supplements)?,
                "approval supplement roster differs"
            );
        } else {
            let raw=documents.execution_authorization_json.as_ref()
                .context("opaque historical approval requires separate explicit desktop execution authorization")?;
            let a: ExecutionAuthorization = serde_json::from_str(raw)?;
            ensure!(
                a.protocol == 1,
                "unsupported desktop execution authorization protocol"
            );
            for h in [&a.approval_blake3, &a.source_binding, &a.policy_blake3] {
                hash(h)?;
            }
            reason(&a.authorization)?;
            ensure!(
                a.approval_blake3 == actual
                    && a.source_binding == seal.binding_blake3()?
                    && a.policy_blake3 == policy_blake3
                    && same(&a.destination, &destination)?
                    && scope(a.scope) == seal.approval.scope,
                "legacy desktop execution binding differs"
            );
        }
        // A typed pin cannot be silently omitted from the actual policy. Full
        // generic evidence validation is still the core importer's authority.
        ensure!(
            seal.supplements.len() == policy.supplements.len(),
            "supplement policy coverage differs"
        );
        for pin in &seal.supplements {
            let matches = policy
                .supplements
                .iter()
                .filter(|p| {
                    p.capture_revision == pin.revision
                        && p.source_id == pin.source_id
                        && matches!(
                            p.origin,
                            crate::catalog_migration::file_metadata::Origin::Embedded
                        )
                        && pin.origin == "embedded"
                })
                .count();
            ensure!(
                matches == 1,
                "supplement policy member missing or duplicated"
            );
        }
        // Stream the exact existing JSON encoding into BLAKE3. Worst-case
        // escaping must not allocate another six times the admitted bundle.
        let token = serialized_hash(&("desktop-lightroom-execution-v1", &documents, &destination))?;
        Ok(Self {
            documents,
            seal,
            policy,
            destination,
            policy_blake3,
            token,
        })
    }
    pub fn documents(&self) -> &Documents {
        &self.documents
    }
    pub fn seal(&self) -> &InputSeal {
        &self.seal
    }
    pub fn policy(&self) -> &Policy {
        &self.policy
    }
    pub fn destination(&self) -> &NativePath {
        &self.destination
    }
    pub fn policy_blake3(&self) -> &str {
        &self.policy_blake3
    }
    pub fn token(&self) -> &str {
        &self.token
    }
    /// Call only in the owned migration helper. This opens the explicitly sealed
    /// inspection copy, never recorded originals/capture paths from its rows.
    pub fn open_source(
        &self,
        limits: ReadLimits,
        cancel: Arc<AtomicBool>,
    ) -> Result<MigrationSource> {
        MigrationSource::open_cancellable(self.seal.clone(), limits, cancel)
    }
}

#[cfg(test)]
mod tests;

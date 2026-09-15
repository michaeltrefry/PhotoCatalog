//! Closed export-worker stage protocol. Artifact names and stage paths are
//! selected only by F; callers carry identities and bounded bytes.
use super::{LeaseId, RootCapability};
use crate::{
    application::U64,
    catalog_exports::ExportWork,
    image_export::EncodingReport,
    metadata_export::SealedPhotoExport,
    photo_render::{PhotoRenderLimits, PhotoRenderTimings},
    storage_volume::NativePath,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub const CHUNK_BYTES: usize = 16 * 1024;
pub const REQUEST_BYTES: usize = 256 * 1024;
pub const RECEIPT_BYTES: usize = 64 * 1024;
pub const BLOB_BYTES: u64 = 16 * 1024 * 1024;
pub const PLAN_BYTES: usize = 128 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub job: String,
    pub sequence: i64,
    pub attempt: String,
    pub authority: String,
    pub renderer: String,
}
impl Binding {
    pub fn from_work(work: &ExportWork) -> Self {
        Self {
            job: work.job.clone(),
            sequence: work.sequence,
            attempt: work.attempt.clone(),
            authority: work.authority.clone(),
            renderer: work.plan.renderer_identity.clone(),
        }
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.job.is_empty() && self.job.len() <= 128,
            "export stage job bound"
        );
        ensure!(self.sequence >= 0, "export stage sequence bound");
        ensure!(
            self.attempt.len() <= 128 && !self.attempt.is_empty(),
            "export stage attempt bound"
        );
        hash(&self.authority)?;
        ensure!(
            !self.renderer.is_empty() && self.renderer.len() <= 256,
            "export stage renderer bound"
        );
        Ok(())
    }
    pub fn validate_work(&self, work: &ExportWork) -> Result<()> {
        ensure!(
            self == &Self::from_work(work),
            "export stage work binding mismatch"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Blob {
    pub bytes: U64,
    pub digest: String,
}
impl Blob {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.bytes.0 <= BLOB_BYTES, "export stage blob limit");
        hash(&self.digest)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    content = "detail",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum NativeTerminal {
    Succeeded,
    Failed { code: Option<i32> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "action",
    content = "arguments",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Action {
    Begin {
        #[serde(with = "exact_work")]
        work: Box<ExportWork>,
        limits: PhotoRenderLimits,
    },
    UploadIcc {
        offset: U64,
        #[serde(skip)]
        bytes: Vec<u8>,
    },
    UploadXmp {
        offset: U64,
        #[serde(skip)]
        bytes: Vec<u8>,
    },
    Ready {
        icc: Option<Blob>,
        xmp: Option<Blob>,
    },
    Arm {
        native: U64,
    },
    NativeDrained {
        native: U64,
        terminal: NativeTerminal,
    },
    ResultAndSeal,
    Abort,
    Release,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub root: RootCapability,
    pub executor: LeaseId,
    pub stage: LeaseId,
    pub operation: U64,
    #[serde(default)]
    pub supervisor: bool,
    pub binding: Binding,
    pub action: Action,
}
impl Request {
    pub fn validate(&self) -> Result<()> {
        super::validate_path(&self.root.canonical_root)?;
        self.root.root_physical.validate()?;
        self.root.catalog_physical.validate()?;
        self.binding.validate()?;
        ensure!(self.operation.0 > 0, "zero export stage operation");
        ensure!(
            self.privileged() == self.supervisor,
            "export native transition requires supervisor"
        );
        match &self.action {
            Action::Begin { work, .. } => self.binding.validate_work(work)?,
            Action::UploadIcc { bytes, .. } | Action::UploadXmp { bytes, .. } => ensure!(
                !bytes.is_empty() && bytes.len() <= CHUNK_BYTES,
                "export stage upload chunk limit"
            ),
            Action::Ready { icc, xmp } => {
                if let Some(blob) = icc {
                    blob.validate()?;
                }
                if let Some(blob) = xmp {
                    blob.validate()?;
                }
            }
            Action::Arm { native } | Action::NativeDrained { native, .. } => {
                ensure!(native.0 > 0, "zero export native operation")
            }
            _ => {}
        }
        // Count the full nested metadata plus raw chunk through the same packer
        // used by F. The surrounding relay applies its configured pool again.
        crate::catalog_session::preview_io::pack(
            self,
            (!self.binary().is_empty()).then(|| self.binary()),
            crate::filesystem_worker::wire::MESSAGE_BYTES,
        )?;
        Ok(())
    }
    pub fn privileged(&self) -> bool {
        matches!(
            self.action,
            Action::Arm { .. } | Action::NativeDrained { .. }
        )
    }
    pub fn cleanup(&self) -> bool {
        matches!(
            self.action,
            Action::NativeDrained { .. } | Action::Abort | Action::Release
        )
    }
    pub fn binary(&self) -> &[u8] {
        match &self.action {
            Action::UploadIcc { bytes, .. } | Action::UploadXmp { bytes, .. } => bytes,
            _ => &[],
        }
    }
    pub fn set_binary(&mut self, bytes: Vec<u8>) -> Result<()> {
        match &mut self.action {
            Action::UploadIcc { bytes: value, .. } | Action::UploadXmp { bytes: value, .. } => {
                *value = bytes
            }
            _ => ensure!(bytes.is_empty(), "unexpected export stage binary"),
        }
        Ok(())
    }
    pub fn digest(&self) -> Result<[u8; 32]> {
        let metadata = crate::filesystem_worker::wire::encode(self, super::ENVELOPE_BYTES)?;
        let mut digest = blake3::Hasher::new();
        digest.update(&metadata);
        digest.update(self.binary());
        Ok(*digest.finalize().as_bytes())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Completion {
    pub authority: String,
    pub attempt: String,
    pub sealed: SealedPhotoExport,
    pub rendered: Rendered,
    pub seal_ms: f64,
    pub peak_resident_bytes: Option<u64>,
    pub peak_method: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rendered {
    pub staging: NativePath,
    pub encoding: EncodingReport,
    pub renderer_identity: String,
    pub metadata_notes: Vec<String>,
    pub timings: PhotoRenderTimings,
}
impl From<crate::export_worker::CompletedExport> for Completion {
    fn from(value: crate::export_worker::CompletedExport) -> Self {
        Self {
            authority: value.authority,
            attempt: value.attempt,
            sealed: value.sealed,
            rendered: Rendered {
                staging: NativePath::from_path(&value.rendered.staging),
                encoding: value.rendered.encoding,
                renderer_identity: value.rendered.renderer_identity,
                metadata_notes: value.rendered.metadata_notes,
                timings: value.rendered.timings,
            },
            seal_ms: value.seal_ms,
            peak_resident_bytes: value.peak_resident_bytes,
            peak_method: value.peak_method,
        }
    }
}
impl Completion {
    fn validate(&self, request: &Request, path: &NativePath) -> Result<()> {
        ensure!(
            self.authority == request.binding.authority
                && self.attempt == request.binding.attempt
                && self.sealed.authority_digest == request.binding.authority
                && self.rendered.renderer_identity == request.binding.renderer
                && self.rendered.staging.to_path()? == path.to_path()?.join("output")
                && self.rendered.encoding.encoded_extent == self.sealed.payload.bytes,
            "export stage completion binding mismatch"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
#[expect(
    clippy::large_enum_variant,
    reason = "the completed proof remains inline in the bounded stage protocol value"
)]
pub enum Value {
    Begun,
    Unit,
    Ready {
        path: NativePath,
    },
    Completed {
        path: NativePath,
        completion: Completion,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub epoch: LeaseId,
    pub session: LeaseId,
    pub stage: LeaseId,
    pub operation: U64,
    pub binding: Binding,
    pub value: Value,
}
impl Reply {
    pub fn validate(&self, request: &Request) -> Result<()> {
        request.validate()?;
        ensure!(
            self.epoch == request.root.epoch
                && self.session == request.root.session
                && self.stage == request.stage
                && self.operation == request.operation
                && self.binding == request.binding,
            "export stage reply binding mismatch"
        );
        match (&request.action, &self.value) {
            (Action::Begin { .. }, Value::Begun)
            | (Action::UploadIcc { .. }, Value::Unit)
            | (Action::UploadXmp { .. }, Value::Unit)
            | (Action::Arm { .. }, Value::Unit)
            | (Action::NativeDrained { .. }, Value::Unit)
            | (Action::Abort, Value::Unit)
            | (Action::Release, Value::Unit) => {}
            (Action::Ready { .. }, Value::Ready { path }) => super::validate_path(path)?,
            (Action::ResultAndSeal, Value::Completed { path, completion }) => {
                super::validate_path(path)?;
                completion.validate(request, path)?;
            }
            _ => anyhow::bail!("unexpected export stage reply"),
        }
        ensure!(
            crate::filesystem_worker::wire::encode(
                self,
                crate::filesystem_worker::wire::MESSAGE_BYTES
            )?
            .len()
                <= crate::filesystem_worker::wire::MESSAGE_BYTES,
            "export stage relay envelope limit"
        );
        Ok(())
    }
}

fn hash(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "export stage digest"
    );
    Ok(())
}

// ExportWork's legacy RawValue serialization normalizes outer whitespace.
// This protocol owns the exact persisted plan bytes, just like N protocol v2.
mod exact_work {
    use super::*;
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct WireWork {
        job: String,
        sequence: i64,
        attempt: String,
        authority: String,
        plan_json: String,
    }
    pub fn serialize<S: serde::Serializer>(
        work: &ExportWork,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Borrowed<'a> {
            job: &'a str,
            sequence: i64,
            attempt: &'a str,
            authority: &'a str,
            plan_json: &'a str,
        }
        Borrowed {
            job: &work.job,
            sequence: work.sequence,
            attempt: &work.attempt,
            authority: &work.authority,
            plan_json: work.plan.raw(),
        }
        .serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Box<ExportWork>, D::Error> {
        let work = WireWork::deserialize(deserializer)?;
        let plan = crate::catalog_exports::checked_plan(&work.plan_json, &work.authority)
            .map_err(serde::de::Error::custom)?;
        Ok(Box::new(ExportWork {
            job: work.job,
            sequence: work.sequence,
            attempt: work.attempt,
            authority: work.authority,
            plan,
        }))
    }
}

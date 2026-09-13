//! Versioned transport only; the renderer's source and identity are unchanged.
use super::*;
use crate::{
    image_export::EncodingReport, metadata_export::wire::StoredPath,
    photo_render::PhotoRenderTimings, storage_volume::NativePath,
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Work2 {
    job: String,
    sequence: i64,
    attempt: String,
    authority: String,
    // String custody retains even whitespace outside the root JSON object.
    plan_json: String,
}
impl Serialize for Request {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Envelope<'a, W> {
            version: u32,
            work: W,
            limits: &'a PhotoRenderLimits,
        }
        match self.version {
            1 => Envelope {
                version: 1,
                work: &self.work,
                limits: &self.limits,
            }
            .serialize(serializer),
            2 => Envelope {
                version: 2,
                work: Work2 {
                    job: self.work.job.clone(),
                    sequence: self.work.sequence,
                    attempt: self.work.attempt.clone(),
                    authority: self.work.authority.clone(),
                    plan_json: self.work.plan.raw().to_owned(),
                },
                limits: &self.limits,
            }
            .serialize(serializer),
            _ => Err(serde::ser::Error::custom(
                "unsupported export worker protocol",
            )),
        }
    }
}
impl<'de> Deserialize<'de> for Request {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Envelope {
            version: u32,
            work: Box<serde_json::value::RawValue>,
            limits: PhotoRenderLimits,
        }
        let e = Envelope::deserialize(deserializer)?;
        let work = match e.version {
            1 => serde_json::from_str(e.work.get()).map_err(serde::de::Error::custom)?,
            2 => {
                let w: Work2 =
                    serde_json::from_str(e.work.get()).map_err(serde::de::Error::custom)?;
                let plan = crate::catalog_exports::checked_plan(&w.plan_json, &w.authority)
                    .map_err(serde::de::Error::custom)?;
                ExportWork {
                    job: w.job,
                    sequence: w.sequence,
                    attempt: w.attempt,
                    authority: w.authority,
                    plan,
                }
            }
            _ => {
                return Err(serde::de::Error::custom(
                    "unsupported export worker protocol",
                ));
            }
        };
        Ok(Self {
            version: e.version,
            work,
            limits: e.limits,
        })
    }
}

fn legacy() -> u32 {
    1
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Completion {
    #[serde(default = "legacy")]
    version: u32,
    authority: String,
    attempt: String,
    sealed: SealedPhotoExport,
    rendered: Rendered,
    seal_ms: f64,
    peak_resident_bytes: Option<u64>,
    peak_method: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rendered {
    staging: StoredPath,
    encoding: EncodingReport,
    renderer_identity: String,
    metadata_notes: Vec<String>,
    timings: PhotoRenderTimings,
}
impl Serialize for CompletedExport {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        Completion {
            version: 2,
            authority: self.authority.clone(),
            attempt: self.attempt.clone(),
            sealed: self.sealed.clone(),
            rendered: Rendered {
                staging: StoredPath::Native(NativePath::from_path(&self.rendered.staging)),
                encoding: self.rendered.encoding.clone(),
                renderer_identity: self.rendered.renderer_identity.clone(),
                metadata_notes: self.rendered.metadata_notes.clone(),
                timings: self.rendered.timings.clone(),
            },
            seal_ms: self.seal_ms,
            peak_resident_bytes: self.peak_resident_bytes,
            peak_method: self.peak_method.clone(),
        }
        .serialize(serializer)
    }
}
impl TryFrom<Completion> for CompletedExport {
    type Error = anyhow::Error;
    fn try_from(c: Completion) -> Result<Self> {
        ensure!(
            matches!(c.version, 1 | 2),
            "unsupported export completion version"
        );
        ensure!(
            c.version != 1 || c.sealed.version == 1,
            "legacy completion requires legacy seal"
        );
        Ok(Self {
            authority: c.authority,
            attempt: c.attempt,
            sealed: c.sealed,
            rendered: StagedPhoto {
                staging: c.rendered.staging.local(c.version == 2)?,
                encoding: c.rendered.encoding,
                renderer_identity: c.rendered.renderer_identity,
                metadata_notes: c.rendered.metadata_notes,
                timings: c.rendered.timings,
            },
            seal_ms: c.seal_ms,
            peak_resident_bytes: c.peak_resident_bytes,
            peak_method: c.peak_method,
        })
    }
}

//! Versioned, catalog-independent photo adjustments.
mod recipe;
pub use recipe::{
    AdjustmentGroup, NoiseReduction, NormalizedRect, Recipe, RecipeV1, Sharpening, ValidatedRecipe,
    WhiteBalance,
};

/// Invalid persisted/user recipe. This does not represent missing source data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipeError(pub String);
impl std::fmt::Display for RecipeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for RecipeError {}

pub(crate) mod color;
mod detail;
pub(crate) mod geometry;
mod render;
pub(crate) use render::verify_original_fingerprint;
pub use render::{
    EditedLinearImage, OriginalRequest, PreparedLinearInput, RenderPurpose, decode_original,
    prepare_linear_proxy, render_recipe, renderer_identity,
};
use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub enum RenderError {
    InvalidRecipe(RecipeError),
    InvalidInput(String),
    InvalidOutput(String),
    InvalidProfile(String),
    InvalidMetadata(String),
    SourceMissing,
    SourceChanged,
    Unsupported(String),
    ResourceLimit {
        resource: &'static str,
        required: u64,
        limit: u64,
    },
    Canceled,
    Decode(crate::media::DecodeError),
    Codec(String),
    Io(std::io::Error),
}
impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for RenderError {}
impl From<std::io::Error> for RenderError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
pub trait CancelCheck: Sync {
    fn is_canceled(&self) -> bool;
    fn check(&self) -> Result<(), RenderError> {
        if self.is_canceled() {
            Err(RenderError::Canceled)
        } else {
            Ok(())
        }
    }
}
impl CancelCheck for std::sync::atomic::AtomicBool {
    fn is_canceled(&self) -> bool {
        self.load(std::sync::atomic::Ordering::Relaxed)
    }
}
impl CancelCheck for () {
    fn is_canceled(&self) -> bool {
        false
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderLimits {
    pub max_pixels: u64,
    pub max_allocation_bytes: u64,
    pub max_live_bytes: u64,
}
impl Default for RenderLimits {
    fn default() -> Self {
        Self {
            max_pixels: 100_000_000,
            max_allocation_bytes: 2 * 1024 * 1024 * 1024,
            max_live_bytes: 4 * 1024 * 1024 * 1024,
        }
    }
}
impl RenderLimits {
    pub(crate) fn admit(self, w: u32, h: u32, surfaces: u64) -> Result<(), RenderError> {
        let pixels = u64::from(w) * u64::from(h);
        if w == 0 || h == 0 || w > 40000 || h > 40000 {
            return Err(RenderError::InvalidOutput(
                "image dimensions outside supported bounds".into(),
            ));
        }
        for (resource, required, limit) in [
            ("pixels", pixels, self.max_pixels.min(100_000_000)),
            ("allocation", pixels * 16, self.max_allocation_bytes),
            (
                "live pixel surfaces",
                pixels * 16 * surfaces,
                self.max_live_bytes,
            ),
        ] {
            if required > limit {
                return Err(RenderError::ResourceLimit {
                    resource,
                    required,
                    limit,
                });
            }
        }
        Ok(())
    }
}
pub(crate) fn buffer(count: usize, limits: RenderLimits) -> Result<Vec<[f32; 4]>, RenderError> {
    let bytes = (count as u64)
        .checked_mul(16)
        .ok_or_else(|| RenderError::InvalidInput("pixel length overflow".into()))?;
    if bytes > limits.max_allocation_bytes {
        return Err(RenderError::ResourceLimit {
            resource: "allocation",
            required: bytes,
            limit: limits.max_allocation_bytes,
        });
    }
    let mut out = Vec::new();
    out.try_reserve_exact(count)
        .map_err(|_| RenderError::ResourceLimit {
            resource: "allocator",
            required: bytes,
            limit: limits.max_allocation_bytes,
        })?;
    out.resize(count, [0.0; 4]);
    Ok(out)
}
#[cfg(test)]
pub(crate) use render::fixture;

pub use render::{
    PreparedProxyExpectation, PreparedProxyIdentity, PreparedProxyReceipt, read_prepared_proxy,
    write_prepared_proxy,
};

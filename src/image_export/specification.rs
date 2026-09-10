use crate::edit::{EditedLinearImage, RenderError, RenderLimits};
use lcms2::{ColorSpaceSignature, Profile, Tag, TagSignature};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegerDepth {
    Eight,
    Sixteen,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TiffDepth {
    Eight,
    Sixteen,
    Float32,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "format", rename_all = "snake_case", deny_unknown_fields)]
pub enum OutputFormat {
    Jpeg { quality: u8 },
    Png { depth: IntegerDepth },
    Tiff { depth: TiffDepth },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum OutputSize {
    Original,
    Fit {
        width: u32,
        height: u32,
        allow_upscale: bool,
    },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OutputProfile {
    Srgb,
    LinearSrgb,
    Icc { bytes: Vec<u8> },
}
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum AlphaPolicy {
    Preserve,
    Composite { linear_rgb: [f32; 3] },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputSpec {
    pub size: OutputSize,
    pub format: OutputFormat,
    pub profile: OutputProfile,
    pub alpha: AlphaPolicy,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputDescriptor {
    pub width: u32,
    pub height: u32,
    pub channels: u8,
    pub bits_per_sample: u8,
    pub floating_point: bool,
    pub orientation: u8,
    pub icc_blake3: String,
    pub integer_clips_to_unit_range: bool,
    pub alpha: AlphaPolicy,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EncodeLimits {
    pub render: RenderLimits,
    pub max_metadata_bytes: u64,
    pub row_buffer_bytes: u64,
}
impl Default for EncodeLimits {
    fn default() -> Self {
        Self {
            render: RenderLimits::default(),
            max_metadata_bytes: 16 * 1024 * 1024,
            row_buffer_bytes: 16 * 1024 * 1024,
        }
    }
}
pub(crate) fn output_profile(
    spec: &OutputProfile,
) -> Result<(Profile, Vec<u8>, bool), RenderError> {
    let (p, supplied) = match spec {
        OutputProfile::Srgb => (Profile::new_srgb(), None),
        OutputProfile::LinearSrgb => (
            crate::media::linear_profile()
                .map_err(|e| RenderError::InvalidProfile(e.to_string()))?,
            None,
        ),
        OutputProfile::Icc { bytes } => {
            if bytes.len() > 16 * 1024 * 1024 {
                return Err(RenderError::ResourceLimit {
                    resource: "ICC bytes",
                    required: bytes.len() as u64,
                    limit: 16 * 1024 * 1024,
                });
            }
            (
                Profile::new_icc(bytes).map_err(|e| RenderError::InvalidProfile(e.to_string()))?,
                Some(bytes.clone()),
            )
        }
    };
    if p.color_space() != ColorSpaceSignature::RgbData {
        return Err(RenderError::InvalidProfile(
            "RGB output ICC required".into(),
        ));
    }
    let linear = p.is_matrix_shaper()
        && [
            TagSignature::RedTRCTag,
            TagSignature::GreenTRCTag,
            TagSignature::BlueTRCTag,
        ]
        .into_iter()
        .all(|t| matches!(p.read_tag(t),Tag::ToneCurve(c) if c.is_linear()));
    let bytes = match supplied {
        Some(b) => b,
        None => {
            let mut b = p
                .icc()
                .map_err(|e| RenderError::InvalidProfile(e.to_string()))?;
            // Built-in profiles have a fixed creation date and unspecified profile ID;
            // LCMS otherwise inserts wall time, changing an unchanged output identity.
            b[24..36].copy_from_slice(&[7, 208, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0]);
            b[84..100].fill(0);
            b
        }
    };
    Ok((p, bytes, linear))
}
pub fn describe_output(
    image: &EditedLinearImage,
    spec: &OutputSpec,
) -> Result<OutputDescriptor, RenderError> {
    if !image.exact() {
        return Err(RenderError::InvalidInput(
            "derivative export requires exact original recipe result".into(),
        ));
    }
    let source = image.as_rendered();
    let (w, h) = match spec.size {
        OutputSize::Original => (source.width, source.height),
        OutputSize::Fit {
            width,
            height,
            allow_upscale,
        } => crate::edit::geometry::fit(source.width, source.height, width, height, allow_upscale)?,
    };
    if w == 0 || h == 0 || w > 40000 || h > 40000 || u64::from(w) * u64::from(h) > 100_000_000 {
        return Err(RenderError::InvalidOutput(
            "output exceeds 100 MP/40000 edge contract".into(),
        ));
    }
    if let AlphaPolicy::Composite { linear_rgb } = spec.alpha {
        if linear_rgb
            .iter()
            .any(|v| !v.is_finite() || !(0.0..=1.0).contains(v))
        {
            return Err(RenderError::InvalidOutput(
                "background must be finite linear RGB in [0,1]".into(),
            ));
        }
    }
    let (bits, float) = match spec.format {
        OutputFormat::Jpeg { quality } => {
            if !(1..=100).contains(&quality) || matches!(spec.alpha, AlphaPolicy::Preserve) {
                return Err(RenderError::InvalidOutput(
                    "JPEG requires quality 1..100 and an explicit background".into(),
                ));
            }
            (8, false)
        }
        OutputFormat::Png { depth } => (if depth == IntegerDepth::Eight { 8 } else { 16 }, false),
        OutputFormat::Tiff { depth } => match depth {
            TiffDepth::Eight => (8, false),
            TiffDepth::Sixteen => (16, false),
            TiffDepth::Float32 => (32, true),
        },
    };
    let (_, icc, linear) = output_profile(&spec.profile)?;
    if float && !linear {
        return Err(RenderError::InvalidProfile(
            "float TIFF requires a linear matrix RGB profile to preserve signed and extended range"
                .into(),
        ));
    }
    Ok(OutputDescriptor {
        width: w,
        height: h,
        channels: if matches!(spec.alpha, AlphaPolicy::Preserve) {
            4
        } else {
            3
        },
        bits_per_sample: bits,
        floating_point: float,
        orientation: 1,
        icc_blake3: blake3::hash(&icc).to_hex().to_string(),
        integer_clips_to_unit_range: !float,
        alpha: spec.alpha,
    })
}

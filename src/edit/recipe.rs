use super::RecipeError;
use serde::{Deserialize, Serialize};

/// The version fixes operation order, units and canonical field order. Unknown
/// versions and fields fail deserialization instead of dropping adjustments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "version", content = "settings", deny_unknown_fields)]
pub enum Recipe {
    #[serde(rename = "1")]
    V1(RecipeV1),
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeV1 {
    pub crop: Option<NormalizedRect>,
    pub straighten_degrees: f32,
    pub exposure_ev: f32,
    pub white_balance: WhiteBalance,
    pub contrast: f32,
    pub highlights: f32,
    pub shadows: f32,
    pub saturation: f32,
    pub vibrance: f32,
    pub sharpening: Sharpening,
    pub noise_reduction: NoiseReduction,
}
/// Normalized edges in the physically oriented original-size canvas. Rotation
/// keeps that extent, rotates about its center, and introduces transparent corners. Crop never silently zooms to an
/// inscribed rectangle; exports preserve alpha or use an explicit background.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizedRect {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum WhiteBalance {
    AsShot,
    TemperatureTint { kelvin: u32, tint: f32 },
}
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sharpening {
    pub amount: f32,
    pub radius_px: f32,
}
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoiseReduction {
    pub luminance: f32,
    pub chroma: f32,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdjustmentGroup {
    Geometry,
    Exposure,
    WhiteBalance,
    Tone,
    Color,
    Sharpening,
    NoiseReduction,
}

/// Immutable validated recipe; canonical bytes contain no catalog/variant IDs.
/// Digest binds the version and all normalized settings, including neutral ones.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedRecipe {
    recipe: Recipe,
    canonical: Vec<u8>,
    digest: String,
}
impl ValidatedRecipe {
    pub fn recipe(&self) -> &Recipe {
        &self.recipe
    }
    pub fn settings(&self) -> &RecipeV1 {
        let Recipe::V1(value) = &self.recipe;
        value
    }
    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical
    }
    pub fn digest(&self) -> &str {
        &self.digest
    }
    /// Pixel edges round to nearest (half upward). Report a collapsed crop on
    /// the target source rather than expanding it during adjustment transfer.
    pub fn validate_dimensions(&self, width: u32, height: u32) -> Result<(u32, u32), RecipeError> {
        if width == 0
            || height == 0
            || width > 40000
            || height > 40000
            || u64::from(width) * u64::from(height) > 100_000_000
        {
            return Err(RecipeError(
                "source dimensions must be nonzero and within 100 MP/40000 per edge".into(),
            ));
        }
        if let Some(c) = self.settings().crop {
            let w = (f64::from(c.right) * f64::from(width)).round() as u32
                - (f64::from(c.left) * f64::from(width)).round() as u32;
            let h = (f64::from(c.bottom) * f64::from(height)).round() as u32
                - (f64::from(c.top) * f64::from(height)).round() as u32;
            if w == 0 || h == 0 {
                return Err(RecipeError(
                    "crop collapses at this source resolution".into(),
                ));
            }
            Ok((w, h))
        } else {
            Ok((width, height))
        }
    }
}
impl Default for Recipe {
    fn default() -> Self {
        Self::V1(RecipeV1::default())
    }
}
impl Default for RecipeV1 {
    fn default() -> Self {
        Self {
            crop: None,
            straighten_degrees: 0.0,
            exposure_ev: 0.0,
            white_balance: WhiteBalance::AsShot,
            contrast: 0.0,
            highlights: 0.0,
            shadows: 0.0,
            saturation: 0.0,
            vibrance: 0.0,
            sharpening: Sharpening {
                amount: 0.0,
                radius_px: 1.0,
            },
            noise_reduction: NoiseReduction {
                luminance: 0.0,
                chroma: 0.0,
            },
        }
    }
}
fn checked(name: &str, value: &mut f32, min: f32, max: f32) -> Result<(), RecipeError> {
    if !value.is_finite() || *value < min || *value > max {
        return Err(RecipeError(format!(
            "{name} must be finite in [{min}, {max}]"
        )));
    }
    if *value == 0.0 {
        *value = 0.0;
    } // One canonical representation of signed zero.
    Ok(())
}
impl Recipe {
    pub fn validate(&self) -> Result<ValidatedRecipe, RecipeError> {
        let mut recipe = self.clone();
        let Self::V1(r) = &mut recipe;
        checked("straighten_degrees", &mut r.straighten_degrees, -45.0, 45.0)?;
        checked("exposure_ev", &mut r.exposure_ev, -10.0, 10.0)?;
        for (name, value) in [
            ("contrast", &mut r.contrast),
            ("highlights", &mut r.highlights),
            ("shadows", &mut r.shadows),
            ("saturation", &mut r.saturation),
            ("vibrance", &mut r.vibrance),
        ] {
            checked(name, value, -1.0, 1.0)?;
        }
        checked("sharpening.amount", &mut r.sharpening.amount, 0.0, 2.0)?;
        checked(
            "sharpening.radius_px",
            &mut r.sharpening.radius_px,
            0.1,
            10.0,
        )?;
        checked(
            "noise_reduction.luminance",
            &mut r.noise_reduction.luminance,
            0.0,
            1.0,
        )?;
        checked(
            "noise_reduction.chroma",
            &mut r.noise_reduction.chroma,
            0.0,
            1.0,
        )?;
        if let WhiteBalance::TemperatureTint { kelvin, tint } = &mut r.white_balance {
            if !(2000..=50000).contains(kelvin) {
                return Err(RecipeError(
                    "white balance kelvin must be 2000..=50000".into(),
                ));
            }
            checked("white_balance.tint", tint, -150.0, 150.0)?;
        }
        if let Some(crop) = &mut r.crop {
            for (name, value) in [
                ("crop.left", &mut crop.left),
                ("crop.top", &mut crop.top),
                ("crop.right", &mut crop.right),
                ("crop.bottom", &mut crop.bottom),
            ] {
                checked(name, value, 0.0, 1.0)?;
            }
            if crop.left >= crop.right || crop.top >= crop.bottom {
                return Err(RecipeError(
                    "crop must have positive width and height".into(),
                ));
            }
        }
        let canonical = serde_json::to_vec(&recipe).map_err(|e| RecipeError(e.to_string()))?;
        let digest = blake3::hash(&canonical).to_hex().to_string();
        Ok(ValidatedRecipe {
            recipe,
            canonical,
            digest,
        })
    }
    /// Copy only explicitly selected groups. Validate both sides first so invalid
    /// discarded fields cannot hide a corrupt source recipe. Duplicate groups
    /// are idempotent; an empty selection returns the validated target unchanged.
    pub fn copy_groups_from(
        &self,
        source: &Recipe,
        groups: &[AdjustmentGroup],
    ) -> Result<ValidatedRecipe, RecipeError> {
        let mut target = self.validate()?.recipe;
        let source = source.validate()?;
        let Self::V1(t) = &mut target;
        let s = source.settings();
        for group in groups {
            match group {
                AdjustmentGroup::Geometry => {
                    t.crop = s.crop;
                    t.straighten_degrees = s.straighten_degrees;
                }
                AdjustmentGroup::Exposure => t.exposure_ev = s.exposure_ev,
                AdjustmentGroup::WhiteBalance => t.white_balance = s.white_balance.clone(),
                AdjustmentGroup::Tone => {
                    t.contrast = s.contrast;
                    t.highlights = s.highlights;
                    t.shadows = s.shadows;
                }
                AdjustmentGroup::Color => {
                    t.saturation = s.saturation;
                    t.vibrance = s.vibrance;
                }
                AdjustmentGroup::Sharpening => t.sharpening = s.sharpening,
                AdjustmentGroup::NoiseReduction => t.noise_reduction = s.noise_reduction,
            }
        }
        target.validate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonical_roundtrip_zero_and_finite_validation() {
        let mut r = RecipeV1::default();
        r.exposure_ev = -0.0;
        let a = Recipe::V1(r.clone()).validate().unwrap();
        let b = Recipe::default().validate().unwrap();
        assert_eq!(a.digest(), b.digest());
        let restored: Recipe = serde_json::from_slice(a.canonical_bytes()).unwrap();
        assert_eq!(restored.validate().unwrap(), a);
        r.shadows = f32::NAN;
        assert!(Recipe::V1(r).validate().is_err());
    }
    #[test]
    fn unknown_versions_fields_and_invalid_geometry_rejected() {
        let mut value = serde_json::to_value(Recipe::default()).unwrap();
        value["version"] = serde_json::json!("2");
        assert!(serde_json::from_value::<Recipe>(value.clone()).is_err());
        value["version"] = serde_json::json!("1");
        value["settings"]["healing"] = serde_json::json!(true);
        assert!(serde_json::from_value::<Recipe>(value).is_err());
        let mut r = RecipeV1::default();
        r.crop = Some(NormalizedRect {
            left: 0.5,
            top: 0.0,
            right: 0.5,
            bottom: 1.0,
        });
        assert!(Recipe::V1(r).validate().is_err());
    }
    #[test]
    fn crop_reports_target_resolution_incompatibility() {
        let mut r = RecipeV1::default();
        r.crop = Some(NormalizedRect {
            left: 0.1,
            top: 0.0,
            right: 0.11,
            bottom: 1.0,
        });
        let v = Recipe::V1(r).validate().unwrap();
        assert!(v.validate_dimensions(2, 2).is_err());
        assert_eq!(v.validate_dimensions(1000, 1000).unwrap(), (10, 1000));
        assert!(v.validate_dimensions(10001, 10000).is_err());
    }
    #[test]
    fn copy_keeps_unselected_groups_and_changes_canonical_identity() {
        let mut source = RecipeV1::default();
        source.exposure_ev = 1.0;
        source.contrast = 0.5;
        let target = Recipe::default();
        let copied = target
            .copy_groups_from(&Recipe::V1(source), &[AdjustmentGroup::Exposure])
            .unwrap();
        assert_eq!(copied.settings().exposure_ev, 1.0);
        assert_eq!(copied.settings().contrast, 0.0);
        assert_ne!(copied.digest(), target.validate().unwrap().digest());
        assert_eq!(
            target.copy_groups_from(copied.recipe(), &[]).unwrap(),
            target.validate().unwrap()
        );
    }
}

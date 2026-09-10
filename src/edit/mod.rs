//! Versioned, catalog-independent photo adjustments.
mod recipe;
pub use recipe::{AdjustmentGroup, NoiseReduction, NormalizedRect, Recipe, RecipeV1, Sharpening, ValidatedRecipe, WhiteBalance};

/// Invalid persisted/user recipe. This does not represent missing source data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipeError(pub String);
impl std::fmt::Display for RecipeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.0) }
}
impl std::error::Error for RecipeError {}

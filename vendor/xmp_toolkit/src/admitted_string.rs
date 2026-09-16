// SPDX-License-Identifier: MIT OR Apache-2.0

/// Serialized XMP whose allocation remains accompanied by an admission guard.
///
/// The text can be borrowed through `Deref` or `AsRef`. Dropping this value frees
/// the text before dropping the guard. No conversion releases the owned text
/// from its guard; a caller that makes another copy must account for that copy.
pub struct AdmittedString<G> {
    text: String,
    // Fields drop in declaration order, so this grant outlives the text buffer.
    _guard: G,
}

impl<G> AdmittedString<G> {
    pub(crate) fn new(text: String, guard: G) -> Self {
        Self {
            text,
            _guard: guard,
        }
    }
}

impl<G> std::ops::Deref for AdmittedString<G> {
    type Target = str;

    fn deref(&self) -> &str {
        &self.text
    }
}

impl<G> AsRef<str> for AdmittedString<G> {
    fn as_ref(&self) -> &str {
        &self.text
    }
}

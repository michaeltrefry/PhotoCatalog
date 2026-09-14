// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{collections::TryReserveError, ffi::CStr};

// Count the exact lossy UTF-8 output while the native allocation is borrowed.
// A raw-byte bound alone misses replacement-character expansion.
pub(super) fn copy(value: &CStr, max_bytes: usize) -> Result<Option<String>, TryReserveError> {
    let mut len = 0usize;
    for chunk in value.to_bytes().utf8_chunks() {
        let replacement = if chunk.invalid().is_empty() { 0 } else { 3 };
        let Some(next) = len
            .checked_add(chunk.valid().len())
            .and_then(|value| value.checked_add(replacement))
            .filter(|next| *next <= max_bytes)
        else {
            return Ok(None);
        };
        len = next;
    }
    let mut output = String::new();
    output.try_reserve_exact(len)?;
    for chunk in value.to_bytes().utf8_chunks() {
        output.push_str(chunk.valid());
        if !chunk.invalid().is_empty() {
            output.push('\u{fffd}');
        }
    }
    Ok(Some(output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn exact_bound_matches_standard_lossy_conversion() {
        let cases: &[&[u8]] = &[
            b"",
            b"plain",
            "café 📷".as_bytes(),
            &[0xff],
            &[0xc2],
            &[0xe2, 0x82],
            &[0xf0, 0x90, 0x80],
            &[0xed, 0xa0, 0x80],
            &[0xf4, 0x90, 0x80, 0x80],
            &[0xe2, b'A', 0xa1],
            &[0xf0, 0x90, b'A', 0xff],
        ];
        for bytes in cases {
            let value = CString::new(*bytes).unwrap();
            let expected = value.to_string_lossy();
            assert_eq!(
                copy(&value, expected.len()).unwrap().as_deref(),
                Some(expected.as_ref())
            );
            if !expected.is_empty() {
                assert!(copy(&value, expected.len() - 1).unwrap().is_none());
            }
        }
        let expanded = CString::new([0xff, 0xff]).unwrap();
        assert!(copy(&expanded, 2).unwrap().is_none());
        assert_eq!(copy(&expanded, 6).unwrap().as_deref(), Some("��"));
    }
}

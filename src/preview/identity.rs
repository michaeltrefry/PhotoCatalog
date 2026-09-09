//! Narrow render identity: UI/catalog source-only changes do not invalidate pixels.
use std::{
    ffi::{CStr, c_char},
    sync::OnceLock,
};
unsafe extern "C" {
    fn libraw_version() -> *const c_char;
    fn JxlDecoderVersion() -> u32;
    fn cmsGetEncodedCMMversion() -> i32;
}
/// Includes the selected build's pixel-producing sources, locked Rust libraries
/// and actual native codec versions. The official DNG SDK pin is in its fetcher.
pub fn renderer_identity() -> &'static str {
    static VALUE: OnceLock<String> = OnceLock::new();
    VALUE.get_or_init(|| {
        let mut hash = blake3::Hasher::new();
        for bytes in [
            include_bytes!("../media.rs").as_slice(),
            include_bytes!("../media/full.rs").as_slice(),
            include_bytes!("../media/psd.rs").as_slice(),
            include_bytes!("../../native/decode.cpp").as_slice(),
            include_bytes!("../../native/decode.h").as_slice(),
            include_bytes!("../../native/dng.cpp").as_slice(),
            include_bytes!("../../native/preview.cpp").as_slice(),
            include_bytes!("../../native/preview.h").as_slice(),
            include_bytes!("../../build.rs").as_slice(),
            include_bytes!("codec.rs").as_slice(),
            include_bytes!("../../Cargo.lock").as_slice(),
            include_bytes!("../../scripts/fetch_dng_sdk.py").as_slice(),
        ] {
            hash.update(bytes);
        }
        hash.update(super::versions().as_bytes());
        hash.update(unsafe { CStr::from_ptr(libraw_version()) }.to_bytes());
        hash.update(&unsafe { JxlDecoderVersion() }.to_le_bytes());
        hash.update(&unsafe { cmsGetEncodedCMMversion() }.to_le_bytes());
        hash.update(std::env::consts::OS.as_bytes());
        hash.update(std::env::consts::ARCH.as_bytes());
        format!("photocatalog-render-3:{}", hash.finalize().to_hex())
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn native_render_identity_is_stable_and_key_sized() {
        let first = super::renderer_identity();
        assert_eq!(first, super::renderer_identity());
        assert!(first.starts_with("photocatalog-render-3:"));
        assert!(first.len() <= 128);
        let digest = first.rsplit_once(':').unwrap().1;
        assert_eq!(digest.len(), 64);
        assert!(digest.bytes().all(|b| b.is_ascii_hexdigit()));
    }
}

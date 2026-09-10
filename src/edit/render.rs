#[path = "prepared.rs"]
mod prepared;
use super::{
    CancelCheck, RecipeError, RenderError, RenderLimits, ValidatedRecipe, WhiteBalance, color,
    detail, geometry,
};
use crate::media::{DecodeLimits, RenderedImage};
pub use prepared::{
    PreparedProxyExpectation, PreparedProxyIdentity, PreparedProxyReceipt, read_prepared_proxy,
    write_prepared_proxy,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Read,
    path::Path,
    sync::OnceLock,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RenderPurpose {
    ExportExact,
    InteractiveProxy { longest_edge: u32 },
}
pub struct OriginalRequest<'a> {
    pub path: &'a Path,
    pub expected_fingerprint: &'a str,
    pub white_balance: &'a WhiteBalance,
}
/// Constructed only by a verified original decode or a proxy derived from it.
/// Public browse RGB/JPEG data cannot manufacture an exact editor input.
pub struct PreparedLinearInput {
    image: RenderedImage,
    source_fingerprint: String,
    white_balance: WhiteBalance,
    renderer_identity: String,
    original_dimensions: (u32, u32),
    exact: bool,
}
impl PreparedLinearInput {
    pub fn width(&self) -> u32 {
        self.image.width
    }
    pub fn height(&self) -> u32 {
        self.image.height
    }
    pub fn pixels(&self) -> &[[f32; 4]] {
        &self.image.pixels
    }
    pub fn source_fingerprint(&self) -> &str {
        &self.source_fingerprint
    }
    pub fn white_balance(&self) -> &WhiteBalance {
        &self.white_balance
    }
    pub fn renderer_identity(&self) -> &str {
        &self.renderer_identity
    }
    pub fn exact(&self) -> bool {
        self.exact
    }
    pub fn original_dimensions(&self) -> (u32, u32) {
        self.original_dimensions
    }
}
pub struct EditedLinearImage {
    pub(crate) image: RenderedImage,
    pub(crate) source_fingerprint: String,
    pub(crate) recipe_digest: String,
    pub(crate) exact: bool,
}
impl EditedLinearImage {
    pub fn as_rendered(&self) -> &RenderedImage {
        &self.image
    }
    pub fn source_fingerprint(&self) -> &str {
        &self.source_fingerprint
    }
    pub fn recipe_digest(&self) -> &str {
        &self.recipe_digest
    }
    pub fn exact(&self) -> bool {
        self.exact
    }
}
pub fn renderer_identity() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        let mut h = blake3::Hasher::new();
        for bytes in [
            include_bytes!("recipe.rs").as_slice(),
            include_bytes!("render.rs").as_slice(),
            include_bytes!("color.rs").as_slice(),
            include_bytes!("geometry.rs").as_slice(),
            include_bytes!("detail.rs").as_slice(),
            include_bytes!("prepared.rs").as_slice(),
            crate::preview::renderer_identity().as_bytes(),
        ] {
            h.update(bytes);
        }
        format!("photocatalog-edit-render-1:{}", h.finalize().to_hex())
    })
}
fn fingerprint_reader(
    reader: &mut impl Read,
    length: u64,
    cancel: &dyn CancelCheck,
) -> Result<String, RenderError> {
    let mut hash = blake3::Hasher::new();
    let mut buffer = [0u8; 65536];
    let mut remaining = length;
    while remaining > 0 {
        cancel.check()?;
        let n = reader.read(&mut buffer[..remaining.min(65536) as usize])?;
        if n == 0 {
            return Err(RenderError::SourceChanged);
        }
        hash.update(&buffer[..n]);
        remaining -= n as u64;
    }
    cancel.check()?;
    if reader.read(&mut buffer[..1])? != 0 {
        return Err(RenderError::SourceChanged);
    }
    Ok(hash.finalize().to_hex().to_string())
}
/// Fixed-buffer verification admits the declared file length before any scan and
/// reads at most that length plus one byte, even if another process grows it.
/// Nonblocking/no-follow opens prevent a replaced FIFO or link from hanging.
pub(crate) fn verify_original_fingerprint(
    path: &Path,
    expected: &str,
    limit: u64,
    cancel: &dyn CancelCheck,
) -> Result<(), RenderError> {
    cancel.check()?;
    let before = fs::symlink_metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            RenderError::SourceMissing
        } else {
            e.into()
        }
    })?;
    if !before.file_type().is_file() {
        return Err(RenderError::InvalidInput(
            "original must be an ordinary file".into(),
        ));
    }
    if before.len() > limit {
        return Err(RenderError::ResourceLimit {
            resource: "original bytes",
            required: before.len(),
            limit,
        });
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000);
    }
    let mut file = options.open(path)?;
    let opened = file.metadata()?;
    if !opened.file_type().is_file() || opened.len() != before.len() {
        return Err(RenderError::SourceChanged);
    }
    if fingerprint_reader(&mut file, opened.len(), cancel)? != expected {
        return Err(RenderError::SourceChanged);
    }
    cancel.check()
}
pub fn decode_original(
    request: OriginalRequest<'_>,
    limits: DecodeLimits,
    cancel: &dyn CancelCheck,
) -> Result<PreparedLinearInput, RenderError> {
    if !request.path.is_absolute() {
        return Err(RenderError::InvalidInput(
            "absolute original path required".into(),
        ));
    }
    verify_original_fingerprint(
        request.path,
        request.expected_fingerprint,
        limits.max_encoded_bytes,
        cancel,
    )?;
    let xy = color::white_point(request.white_balance)?;
    let image = crate::media::decode_with_white_point(request.path, limits, xy)
        .map_err(RenderError::Decode)?;
    cancel.check()?;
    verify_original_fingerprint(
        request.path,
        request.expected_fingerprint,
        limits.max_encoded_bytes,
        cancel,
    )?;
    let original_dimensions = (image.width, image.height);
    Ok(PreparedLinearInput {
        image,
        source_fingerprint: request.expected_fingerprint.into(),
        white_balance: request.white_balance.clone(),
        renderer_identity: renderer_identity().into(),
        original_dimensions,
        exact: true,
    })
}
pub fn prepare_linear_proxy(
    input: &PreparedLinearInput,
    longest_edge: u32,
    limits: RenderLimits,
    cancel: &dyn CancelCheck,
) -> Result<PreparedLinearInput, RenderError> {
    if longest_edge == 0 || longest_edge > 40000 {
        return Err(RenderError::InvalidInput("invalid proxy edge".into()));
    }
    limits.admit(input.width(), input.height(), 2)?;
    let (w, h) = geometry::fit(
        input.width(),
        input.height(),
        longest_edge,
        longest_edge,
        false,
    )?;
    let pixels = geometry::resize(
        input.pixels(),
        input.width(),
        input.height(),
        w,
        h,
        limits,
        cancel,
    )?;
    let image = RenderedImage {
        width: w,
        height: h,
        pixels,
        metadata: input.image.metadata.clone(),
        provenance: input.image.provenance.clone(),
    };
    Ok(PreparedLinearInput {
        image,
        source_fingerprint: input.source_fingerprint.clone(),
        white_balance: input.white_balance.clone(),
        renderer_identity: input.renderer_identity.clone(),
        original_dimensions: input.original_dimensions,
        exact: false,
    })
}
pub fn render_recipe(
    input: &PreparedLinearInput,
    recipe: &ValidatedRecipe,
    purpose: RenderPurpose,
    limits: RenderLimits,
    cancel: &dyn CancelCheck,
) -> Result<EditedLinearImage, RenderError> {
    cancel.check()?;
    if input.renderer_identity != renderer_identity()
        || input.white_balance != recipe.settings().white_balance
    {
        return Err(RenderError::InvalidInput(
            "prepared source WB/renderer differs; decode original with requested white balance"
                .into(),
        ));
    }
    if matches!(purpose, RenderPurpose::ExportExact) && !input.exact {
        return Err(RenderError::InvalidInput(
            "export requires exact original input".into(),
        ));
    }
    recipe.validate_dimensions(input.original_dimensions.0, input.original_dimensions.1)?;
    recipe.validate_dimensions(input.width(), input.height())?;
    limits.admit(input.width(), input.height(), 4)?;
    let mut pixels = super::buffer(input.pixels().len(), limits)?;
    pixels.copy_from_slice(input.pixels());
    let r = recipe.settings();
    detail::denoise(
        &mut pixels,
        input.width(),
        input.height(),
        r.noise_reduction,
        limits,
        cancel,
    )?;
    let exposure = r.exposure_ev.exp2();
    if r.exposure_ev != 0.0
        || r.contrast != 0.0
        || r.highlights != 0.0
        || r.shadows != 0.0
        || r.saturation != 0.0
        || r.vibrance != 0.0
    {
        for row in pixels.chunks_mut(input.width() as usize) {
            cancel.check()?;
            for p in row {
                for c in &mut p[..3] {
                    *c *= exposure;
                }
                let y = color::luma(p);
                let magnitude = y.abs();
                let shadow = 1.0 / (1.0 + 8.0 * magnitude).powi(2);
                let highlight = magnitude / (magnitude + 0.5);
                let tonal = (2.0 * (r.shadows * shadow + r.highlights * highlight)).exp2();
                let contrast = ((magnitude + 0.18) / 0.36).powf(r.contrast * 0.5);
                let gain = tonal * contrast;
                for c in &mut p[..3] {
                    *c *= gain;
                }
                let y = color::luma(p);
                let maximum = p[..3].iter().copied().fold(f32::NEG_INFINITY, f32::max);
                let minimum = p[..3].iter().copied().fold(f32::INFINITY, f32::min);
                let chroma =
                    ((maximum - minimum) / (maximum.abs() + minimum.abs() + 0.18)).clamp(0.0, 1.0);
                let saturation = (1.0 + r.saturation) * (1.0 + r.vibrance * (1.0 - chroma));
                for c in &mut p[..3] {
                    *c = y + (*c - y) * saturation;
                }
            }
        }
    }
    let (mut pixels, mut w, mut h) =
        geometry::straighten_crop(pixels, input.width(), input.height(), r, limits, cancel)?;
    if let RenderPurpose::InteractiveProxy { longest_edge } = purpose {
        if longest_edge == 0 {
            return Err(RenderError::InvalidInput("zero preview edge".into()));
        }
        let (nw, nh) = geometry::fit(w, h, longest_edge, longest_edge, false)?;
        let resized = geometry::resize(&pixels, w, h, nw, nh, limits, cancel)?;
        drop(pixels);
        pixels = resized;
        w = nw;
        h = nh;
    }
    detail::sharpen(&mut pixels, w, h, r.sharpening, limits, cancel)?;
    if pixels
        .iter()
        .any(|p| p.iter().any(|v| !v.is_finite()) || !(0.0..=1.0).contains(&p[3]))
    {
        return Err(RenderError::InvalidInput(
            "recipe produced nonfinite pixels or invalid alpha".into(),
        ));
    }
    let mut provenance = input.image.provenance.clone();
    provenance.pipeline_version = renderer_identity().into();
    provenance.notes.push(format!("Recipe {}: WB at decode; denoise, exposure, tone, color, fixed-canvas straighten/crop, output resize, unsharp mask. Proxy spatial operations are approximate.",recipe.digest()));
    Ok(EditedLinearImage {
        image: RenderedImage {
            metadata: input.image.metadata.clone(),
            width: w,
            height: h,
            pixels,
            provenance,
        },
        source_fingerprint: input.source_fingerprint.clone(),
        recipe_digest: recipe.digest().into(),
        exact: input.exact && matches!(purpose, RenderPurpose::ExportExact),
    })
}
impl From<RecipeError> for RenderError {
    fn from(e: RecipeError) -> Self {
        Self::InvalidRecipe(e)
    }
}

#[cfg(test)]
pub(crate) fn fixture(w: u32, h: u32, pixels: Vec<[f32; 4]>) -> PreparedLinearInput {
    use crate::media::{Metadata, RenderProvenance};
    assert_eq!(pixels.len(), w as usize * h as usize);
    PreparedLinearInput {
        image: RenderedImage {
            width: w,
            height: h,
            pixels,
            metadata: Metadata {
                format: "test linear float".into(),
                width: w,
                height: h,
                orientation: 1,
                camera_make: None,
                camera_model: None,
                captured_at: None,
                lens: None,
                preview_source: "original analytic pixels".into(),
            },
            provenance: RenderProvenance {
                pipeline_version: "fixture".into(),
                decoder: "analytic".into(),
                source_bits_per_channel: 32,
                source_color: "linear sRGB".into(),
                working_color: "linear sRGB".into(),
                alpha: "straight".into(),
                calibration: None,
                spatial_calibration: None,
                notes: vec![],
            },
        },
        source_fingerprint: "test-fingerprint".into(),
        white_balance: WhiteBalance::AsShot,
        renderer_identity: renderer_identity().into(),
        original_dimensions: (w, h),
        exact: true,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::{NoiseReduction, NormalizedRect, Recipe, RecipeV1, Sharpening};
    fn apply(p: &PreparedLinearInput, r: RecipeV1) -> EditedLinearImage {
        render_recipe(
            p,
            &Recipe::V1(r).validate().unwrap(),
            RenderPurpose::ExportExact,
            RenderLimits::default(),
            &(),
        )
        .unwrap()
    }
    #[test]
    fn neutral_is_exact_and_exposure_preserves_signed_hdr_straight_alpha() {
        let p = fixture(2, 1, vec![[4.0, -0.25, 0.5, 0.25], [0.2, 0.3, 0.4, 0.0]]);
        assert_eq!(apply(&p, RecipeV1::default()).image.pixels, p.pixels());
        let out = apply(
            &p,
            RecipeV1 {
                exposure_ev: 1.0,
                ..Default::default()
            },
        );
        assert_eq!(
            out.image.pixels,
            vec![[8.0, -0.5, 1.0, 0.25], [0.4, 0.6, 0.8, 0.0]]
        );
    }
    #[test]
    fn combined_tone_color_matches_separate_f64_equations() {
        let p = fixture(1, 1, vec![[0.08, 0.3, 1.4, 0.3]]);
        let r = RecipeV1 {
            exposure_ev: 0.7,
            contrast: 0.3,
            highlights: -0.4,
            shadows: 0.2,
            saturation: -0.3,
            vibrance: 0.4,
            ..Default::default()
        };
        let actual = apply(&p, r).image.pixels[0];
        let mut e = [0.08f64, 0.3, 1.4].map(|v| v * 2f64.powf(0.7));
        let y = 0.2126 * e[0] + 0.7152 * e[1] + 0.0722 * e[2];
        let gain = 2f64.powf(2.0 * (0.2 / (1.0 + 8.0 * y).powi(2) - 0.4 * y / (y + 0.5)))
            * ((y + 0.18) / 0.36).powf(0.15);
        e = e.map(|v| v * gain);
        let y = 0.2126 * e[0] + 0.7152 * e[1] + 0.0722 * e[2];
        let chroma = (e[2] - e[0]) / (e[2] + e[0] + 0.18);
        let sat = 0.7 * (1.0 + 0.4 * (1.0 - chroma));
        for c in 0..3 {
            let expected = y + (e[c] - y) * sat;
            assert!((f64::from(actual[c]) - expected).abs() < 2e-6);
        }
        assert_eq!(actual[3], 0.3);
    }
    #[test]
    fn crop_fixed_canvas_rotation_and_alpha_aware_resize() {
        let pixels = (0..25).map(|n| [n as f32, 0.0, 0.0, 1.0]).collect();
        let p = fixture(5, 5, pixels);
        let r = RecipeV1 {
            crop: Some(NormalizedRect {
                left: 0.2,
                top: 0.2,
                right: 0.8,
                bottom: 0.8,
            }),
            ..Default::default()
        };
        let cropped = apply(&p, r);
        assert_eq!((cropped.image.width, cropped.image.height), (3, 3));
        assert_eq!(
            cropped
                .image
                .pixels
                .iter()
                .map(|p| p[0])
                .collect::<Vec<_>>(),
            vec![6., 7., 8., 11., 12., 13., 16., 17., 18.]
        );
        let rotated = apply(
            &p,
            RecipeV1 {
                straighten_degrees: 45.,
                ..Default::default()
            },
        );
        assert_eq!((rotated.image.width, rotated.image.height), (5, 5));
        assert!(rotated.image.pixels[0][3] < 1.0);
        assert!((rotated.image.pixels[12][0] - 12.).abs() < 1e-6);
        let pair = fixture(2, 1, vec![[4., -1., 0.5, 1.], [100., 100., 100., 0.]]);
        let proxy = prepare_linear_proxy(&pair, 1, RenderLimits::default(), &()).unwrap();
        assert_eq!(proxy.pixels(), &[[4., -1., 0.5, 0.5]]);
        assert!(
            render_recipe(
                &proxy,
                &Recipe::default().validate().unwrap(),
                RenderPurpose::ExportExact,
                RenderLimits::default(),
                &()
            )
            .is_err()
        );
    }
    #[test]
    fn spatial_controls_retain_flat_hdr_and_reduce_noise_then_sharpen_edges() {
        let flat = fixture(9, 9, vec![[2., -0.2, 0.8, 0.5]; 81]);
        let r = RecipeV1 {
            sharpening: Sharpening {
                amount: 1.2,
                radius_px: 1.5,
            },
            noise_reduction: NoiseReduction {
                luminance: 0.8,
                chroma: 0.7,
            },
            ..Default::default()
        };
        let a = apply(&flat, r);
        for p in a.image.pixels {
            for (v, e) in p.into_iter().zip([2., -0.2, 0.8, 0.5]) {
                assert!((v - e).abs() < 2e-5);
            }
        }
        let mut samples = vec![[0.5, 0.5, 0.5, 1.]; 81];
        samples[40] = [0.6, 0.6, 0.6, 1.];
        let noisy = fixture(9, 9, samples);
        let smooth = apply(
            &noisy,
            RecipeV1 {
                noise_reduction: NoiseReduction {
                    luminance: 1.,
                    chroma: 0.,
                },
                ..Default::default()
            },
        );
        assert!(smooth.image.pixels[40][0] < 0.6);
        let sharp = apply(
            &noisy,
            RecipeV1 {
                sharpening: Sharpening {
                    amount: 1.,
                    radius_px: 1.,
                },
                ..Default::default()
            },
        );
        assert!(sharp.image.pixels[40][0] > 0.6);
        assert_eq!(sharp.image.pixels[40][3], 1.);
    }
    #[test]
    fn admitted_100mp_requires_sufficient_live_allowance_without_allocating() {
        let l = RenderLimits::default();
        assert!(matches!(
            l.admit(10000, 10000, 4),
            Err(RenderError::ResourceLimit { .. })
        ));
        assert!(
            RenderLimits {
                max_live_bytes: 8 * 1024 * 1024 * 1024,
                ..l
            }
            .admit(10000, 10000, 4)
            .is_ok()
        );
        assert!(l.admit(10001, 10000, 1).is_err());
    }
    #[test]
    fn low_admission_cancel_and_wrong_wb_never_produce_result() {
        let p = fixture(3, 2, vec![[1., 0., 0., 1.]; 6]);
        let r = Recipe::default().validate().unwrap();
        assert!(matches!(
            render_recipe(
                &p,
                &r,
                RenderPurpose::ExportExact,
                RenderLimits {
                    max_allocation_bytes: 1,
                    ..Default::default()
                },
                &()
            ),
            Err(RenderError::ResourceLimit { .. })
        ));
        let canceled = std::sync::atomic::AtomicBool::new(true);
        assert!(matches!(
            render_recipe(
                &p,
                &r,
                RenderPurpose::ExportExact,
                RenderLimits::default(),
                &canceled
            ),
            Err(RenderError::Canceled)
        ));
        let r = Recipe::V1(RecipeV1 {
            white_balance: WhiteBalance::TemperatureTint {
                kelvin: 5500,
                tint: 0.,
            },
            ..Default::default()
        })
        .validate()
        .unwrap();
        assert!(
            render_recipe(
                &p,
                &r,
                RenderPurpose::ExportExact,
                RenderLimits::default(),
                &()
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod fingerprint_tests {
    use super::*;
    #[test]
    fn growing_or_short_sources_stop_at_the_declared_revision() {
        struct Endless {
            read: usize,
        }
        impl Read for Endless {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                b.fill(1);
                self.read += b.len();
                Ok(b.len())
            }
        }
        let mut reader = Endless { read: 0 };
        assert!(matches!(
            fingerprint_reader(&mut reader, 17, &()),
            Err(RenderError::SourceChanged)
        ));
        assert_eq!(reader.read, 18);
        assert!(matches!(
            fingerprint_reader(&mut &b"short"[..], 20, &()),
            Err(RenderError::SourceChanged)
        ));
        assert_eq!(
            fingerprint_reader(&mut &b"exact"[..], 5, &()).unwrap(),
            blake3::hash(b"exact").to_hex().as_str()
        );
    }
    #[test]
    fn oversized_original_is_refused_before_the_decoder_and_cancel_before_open() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("large.png");
        std::fs::write(&path, b"four").unwrap();
        let result = decode_original(
            OriginalRequest {
                path: &path,
                expected_fingerprint: &"0".repeat(64),
                white_balance: &WhiteBalance::AsShot,
            },
            DecodeLimits {
                max_encoded_bytes: 3,
                ..Default::default()
            },
            &(),
        );
        assert!(matches!(
            result,
            Err(RenderError::ResourceLimit {
                resource: "original bytes",
                required: 4,
                limit: 3
            })
        ));
        assert!(matches!(
            verify_original_fingerprint(
                &temp.path().join("missing"),
                "",
                1,
                &std::sync::atomic::AtomicBool::new(true)
            ),
            Err(RenderError::Canceled)
        ));
    }
    #[cfg(unix)]
    #[test]
    fn fifo_and_symlink_originals_are_rejected_without_opening_a_stream() {
        use std::os::unix::{ffi::OsStrExt, fs::symlink};
        let temp = tempfile::tempdir().unwrap();
        let fifo = temp.path().join("fifo.png");
        let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(matches!(
            verify_original_fingerprint(&fifo, "", 100, &()),
            Err(RenderError::InvalidInput(_))
        ));
        let real = temp.path().join("real");
        std::fs::write(&real, b"bytes").unwrap();
        let link = temp.path().join("link.png");
        symlink(real, &link).unwrap();
        assert!(matches!(
            verify_original_fingerprint(&link, "", 100, &()),
            Err(RenderError::InvalidInput(_))
        ));
    }
}

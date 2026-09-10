use super::PreparedRgb;
use anyhow::{Result, ensure};
use serde::Serialize;
#[derive(Debug, Serialize)]
pub struct QualityMetrics {
    pub mse_rgb8: f64,
    pub psnr_db: Option<f64>,
    pub identical: bool,
    pub block_rgb_ssim: f64,
    pub maximum_channel_error: u8,
}
/// Descriptive block RGB SSIM, not MS-SSIM: nonoverlapping 8x8 blocks,
/// partial blocks included, population variance, C1=6.5025 and C2=58.5225.
pub fn quality_metrics(reference: &PreparedRgb, decoded: &PreparedRgb) -> Result<QualityMetrics> {
    ensure!(
        reference.width() == decoded.width() && reference.height() == decoded.height(),
        "quality dimensions mismatch"
    );
    let (a, b) = (reference.pixels(), decoded.pixels());
    let mse = a
        .iter()
        .zip(b)
        .map(|(x, y)| (f64::from(*x) - f64::from(*y)).powi(2))
        .sum::<f64>()
        / a.len() as f64;
    let mut total = 0.;
    let mut blocks = 0;
    let w = reference.width() as usize;
    let h = reference.height() as usize;
    for top in (0..h).step_by(8) {
        for left in (0..w).step_by(8) {
            for c in 0..3 {
                let mut sa = 0.;
                let mut sb = 0.;
                let mut aa = 0.;
                let mut bb = 0.;
                let mut ab = 0.;
                let mut n = 0.;
                for y in top..(top + 8).min(h) {
                    for x in left..(left + 8).min(w) {
                        let i = (y * w + x) * 3 + c;
                        let av = f64::from(a[i]);
                        let bv = f64::from(b[i]);
                        sa += av;
                        sb += bv;
                        aa += av * av;
                        bb += bv * bv;
                        ab += av * bv;
                        n += 1.;
                    }
                }
                let ma = sa / n;
                let mb = sb / n;
                let va = (aa / n - ma * ma).max(0.);
                let vb = (bb / n - mb * mb).max(0.);
                let cov = ab / n - ma * mb;
                total += ((2. * ma * mb + 6.5025) * (2. * cov + 58.5225))
                    / ((ma * ma + mb * mb + 6.5025) * (va + vb + 58.5225));
                blocks += 1;
            }
        }
    }
    Ok(QualityMetrics {
        mse_rgb8: mse,
        psnr_db: if mse == 0. {
            None
        } else {
            Some(10. * (255f64.powi(2) / mse).log10())
        },
        identical: mse == 0.,
        block_rgb_ssim: total / f64::from(blocks),
        maximum_channel_error: a.iter().zip(b).map(|(x, y)| x.abs_diff(*y)).max().unwrap(),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn constants_and_partial_blocks_follow_the_declared_math() {
        let a = PreparedRgb::new(9, 9, vec![0; 9 * 9 * 3]).unwrap();
        let b = PreparedRgb::new(9, 9, vec![10; 9 * 9 * 3]).unwrap();
        let identical = quality_metrics(&a, &a).unwrap();
        assert_eq!(identical.block_rgb_ssim, 1.);
        assert!(identical.identical);
        assert_eq!(identical.psnr_db, None);
        let different = quality_metrics(&a, &b).unwrap();
        assert_eq!(different.mse_rgb8, 100.);
        assert_eq!(different.maximum_channel_error, 10);
        assert!((different.block_rgb_ssim - 6.5025 / 106.5025).abs() < 1e-12);
    }
}

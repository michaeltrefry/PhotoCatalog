use super::{CancelCheck, RecipeV1, RenderError, RenderLimits};
pub(crate) fn fit(
    w: u32,
    h: u32,
    mw: u32,
    mh: u32,
    upscale: bool,
) -> Result<(u32, u32), RenderError> {
    if w == 0 || h == 0 || mw == 0 || mh == 0 {
        return Err(RenderError::InvalidOutput("zero output dimensions".into()));
    }
    let scale = (f64::from(mw) / f64::from(w)).min(f64::from(mh) / f64::from(h));
    let scale = if upscale { scale } else { scale.min(1.0) };
    Ok((
        ((w as f64 * scale).round() as u32).max(1),
        ((h as f64 * scale).round() as u32).max(1),
    ))
}
fn accumulate(sum: &mut [f64; 4], p: &[f32; 4], weight: f64) {
    let a = f64::from(p[3]) * weight;
    for c in 0..3 {
        sum[c] += f64::from(p[c]) * a;
    }
    sum[3] += a;
}
fn straight(sum: [f64; 4], normalizer: f64) -> [f32; 4] {
    if sum[3] <= 0.0 {
        return [0.0; 4];
    }
    [
        (sum[0] / sum[3]) as f32,
        (sum[1] / sum[3]) as f32,
        (sum[2] / sum[3]) as f32,
        (sum[3] / normalizer).clamp(0.0, 1.0) as f32,
    ]
}
fn bilinear(p: &[[f32; 4]], w: u32, h: u32, x: f64, y: f64, clamp: bool) -> [f32; 4] {
    let x0 = x.floor() as i64;
    let y0 = y.floor() as i64;
    let fx = x - x.floor();
    let fy = y - y.floor();
    let mut sum = [0.0; 4];
    for (dy, wy) in [(0, 1.0 - fy), (1, fy)] {
        for (dx, wx) in [(0, 1.0 - fx), (1, fx)] {
            let mut xx = x0 + dx;
            let mut yy = y0 + dy;
            if clamp {
                xx = xx.clamp(0, i64::from(w) - 1);
                yy = yy.clamp(0, i64::from(h) - 1);
            }
            if xx >= 0 && yy >= 0 && xx < i64::from(w) && yy < i64::from(h) {
                accumulate(
                    &mut sum,
                    &p[yy as usize * w as usize + xx as usize],
                    wx * wy,
                );
            }
        }
    }
    straight(sum, 1.0)
}
/// Exact area integration for reductions; bilinear for enlargement. Interpolate
/// premultiplied samples but expose straight alpha, without clipping scene RGB.
pub(crate) fn resize(
    p: &[[f32; 4]],
    w: u32,
    h: u32,
    nw: u32,
    nh: u32,
    limits: RenderLimits,
    cancel: &dyn CancelCheck,
) -> Result<Vec<[f32; 4]>, RenderError> {
    limits.admit(nw, nh, 3)?;
    let mut out = super::buffer(nw as usize * nh as usize, limits)?;
    if (w, h) == (nw, nh) {
        out.copy_from_slice(p);
        return Ok(out);
    }
    let sx = f64::from(w) / f64::from(nw);
    let sy = f64::from(h) / f64::from(nh);
    for y in 0..nh {
        cancel.check()?;
        for x in 0..nw {
            out[y as usize * nw as usize + x as usize] = if sx >= 1.0 && sy >= 1.0 {
                let (left, right, top, bottom) = (
                    x as f64 * sx,
                    (x + 1) as f64 * sx,
                    y as f64 * sy,
                    (y + 1) as f64 * sy,
                );
                let mut sum = [0.0; 4];
                for yy in top.floor() as u32..(bottom.ceil() as u32).min(h) {
                    let wy = (bottom.min((yy + 1) as f64) - top.max(yy as f64)).max(0.0);
                    for xx in left.floor() as u32..(right.ceil() as u32).min(w) {
                        let wx = (right.min((xx + 1) as f64) - left.max(xx as f64)).max(0.0);
                        accumulate(
                            &mut sum,
                            &p[yy as usize * w as usize + xx as usize],
                            wx * wy,
                        );
                    }
                }
                straight(sum, sx * sy)
            } else {
                bilinear(
                    p,
                    w,
                    h,
                    (x as f64 + 0.5) * sx - 0.5,
                    (y as f64 + 0.5) * sy - 0.5,
                    true,
                )
            };
        }
    }
    Ok(out)
}
pub(crate) fn straighten_crop(
    p: Vec<[f32; 4]>,
    w: u32,
    h: u32,
    r: &RecipeV1,
    limits: RenderLimits,
    cancel: &dyn CancelCheck,
) -> Result<(Vec<[f32; 4]>, u32, u32), RenderError> {
    let (left, top, right, bottom) = r.crop.map_or((0, 0, w, h), |c| {
        (
            (f64::from(c.left) * w as f64).round() as u32,
            (f64::from(c.top) * h as f64).round() as u32,
            (f64::from(c.right) * w as f64).round() as u32,
            (f64::from(c.bottom) * h as f64).round() as u32,
        )
    });
    let (nw, nh) = (right - left, bottom - top);
    limits.admit(nw, nh, 3)?;
    if r.straighten_degrees == 0.0 && (left, top, nw, nh) == (0, 0, w, h) {
        return Ok((p, w, h));
    }
    let mut out = super::buffer(nw as usize * nh as usize, limits)?;
    let theta = f64::from(r.straighten_degrees).to_radians();
    let (sin, cos) = theta.sin_cos();
    let cx = (w as f64 - 1.0) * 0.5;
    let cy = (h as f64 - 1.0) * 0.5;
    for y in 0..nh {
        cancel.check()?;
        for x in 0..nw {
            out[y as usize * nw as usize + x as usize] = if theta == 0.0 {
                p[(y + top) as usize * w as usize + (x + left) as usize]
            } else {
                let dx = (x + left) as f64 - cx;
                let dy = (y + top) as f64 - cy;
                bilinear(
                    &p,
                    w,
                    h,
                    cos * dx + sin * dy + cx,
                    -sin * dx + cos * dy + cy,
                    false,
                )
            };
        }
    }
    Ok((out, nw, nh))
}

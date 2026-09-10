use super::{color::luma, CancelCheck, NoiseReduction, RenderError, RenderLimits, Sharpening};
/// Fixed 3x3 edge-aware luminance/chroma smoothing. Strength changes the range
/// tolerance and blend independently; alpha remains exactly the source alpha.
pub(crate) fn denoise(p:&mut [[f32;4]],w:u32,h:u32,n:NoiseReduction,limits:RenderLimits,cancel:&dyn CancelCheck)->Result<(),RenderError> {
    if n.luminance==0.0 && n.chroma==0.0 {return Ok(());}
    let mut src=super::buffer(p.len(),limits)?;src.copy_from_slice(p);
    for y in 0..h {cancel.check()?;for x in 0..w {
        let i=y as usize*w as usize+x as usize;let original=src[i];if original[3]==0.0 {continue;}
        let level=luma(&original);let tolerance=0.03+0.25*(level.abs()+0.18)*n.luminance.max(n.chroma);
        let mut sum=[0.0f32;3];let mut weight=0.0;
        for dy in -1i64..=1 {for dx in -1i64..=1 {
            let yy=(i64::from(y)+dy).clamp(0,i64::from(h)-1);let xx=(i64::from(x)+dx).clamp(0,i64::from(w)-1);
            let q=src[yy as usize*w as usize+xx as usize];let delta=(luma(&q)-level)/tolerance;
            let k=q[3]/(1.0+delta*delta);for c in 0..3 {sum[c]+=q[c]*k;}weight+=k;
        }}
        if weight>0.0 {
            let mean=[sum[0]/weight,sum[1]/weight,sum[2]/weight,original[3]];let mean_luma=luma(&mean);
            let output_luma=level+(mean_luma-level)*n.luminance;
            for c in 0..3 {let chroma=original[c]-level;p[i][c]=output_luma+chroma+((mean[c]-mean_luma)-chroma)*n.chroma;}
        }
    }}Ok(())
}
/// Two-pass bounded box approximation used by the V1 unsharp mask. Radius is in
/// final output pixels; fractional radii weight the two boundary samples continuously.
pub(crate) fn sharpen(p:&mut [[f32;4]],w:u32,h:u32,s:Sharpening,limits:RenderLimits,cancel:&dyn CancelCheck)->Result<(),RenderError> {
    if s.amount==0.0 {return Ok(());}
    let radius=s.radius_px.ceil() as i64;let fraction=s.radius_px-(radius-1) as f32;
    let mut horizontal=super::buffer(p.len(),limits)?;let mut blurred=super::buffer(p.len(),limits)?;
    let diameter=(2*radius-1) as f32+2.0*fraction;
    for y in 0..h {
        cancel.check()?;let mut sum=[0.0f32;4];
        for dx in -radius..=radius {let q=p[y as usize*w as usize+dx.clamp(0,i64::from(w)-1) as usize];for c in 0..3 {sum[c]+=q[c]*q[3];}sum[3]+=q[3];}
        for x in 0..w {
            let left=p[y as usize*w as usize+(i64::from(x)-radius).clamp(0,i64::from(w)-1) as usize];
            let right=p[y as usize*w as usize+(i64::from(x)+radius).clamp(0,i64::from(w)-1) as usize];
            for c in 0..4 {let boundary=if c==3 {left[3]+right[3]} else {left[c]*left[3]+right[c]*right[3]};horizontal[y as usize*w as usize+x as usize][c]=(sum[c]-(1.0-fraction)*boundary)/diameter;}
            for (xx,sign) in [(i64::from(x)-radius,-1.0),(i64::from(x)+radius+1,1.0)] {
                let q=p[y as usize*w as usize+xx.clamp(0,i64::from(w)-1) as usize];for c in 0..3 {sum[c]+=sign*q[c]*q[3];}sum[3]+=sign*q[3];
            }
        }
    }
    for x in 0..w {
        if x%64==0 {cancel.check()?;}let mut sum=[0.0f32;4];
        for dy in -radius..=radius {let q=horizontal[dy.clamp(0,i64::from(h)-1) as usize*w as usize+x as usize];for c in 0..4 {sum[c]+=q[c];}}
        for y in 0..h {
            let i=y as usize*w as usize+x as usize;
            let top=horizontal[(i64::from(y)-radius).clamp(0,i64::from(h)-1) as usize*w as usize+x as usize];
            let bottom=horizontal[(i64::from(y)+radius).clamp(0,i64::from(h)-1) as usize*w as usize+x as usize];
            let alpha=sum[3]-(1.0-fraction)*(top[3]+bottom[3]);
            if alpha>0.0 {for c in 0..3 {blurred[i][c]=(sum[c]-(1.0-fraction)*(top[c]+bottom[c]))/alpha;}}
            else {blurred[i]=p[i];}
            for (yy,sign) in [(i64::from(y)-radius,-1.0),(i64::from(y)+radius+1,1.0)] {let q=horizontal[yy.clamp(0,i64::from(h)-1) as usize*w as usize+x as usize];for c in 0..4 {sum[c]+=sign*q[c];}}
        }
    }
    let strength=s.amount;
    for (row,blur) in p.chunks_mut(w as usize).zip(blurred.chunks(w as usize)) {cancel.check()?;for (pixel,mean) in row.iter_mut().zip(blur) {for c in 0..3 {pixel[c]+=strength*(pixel[c]-mean[c]);}}}
    Ok(())
}

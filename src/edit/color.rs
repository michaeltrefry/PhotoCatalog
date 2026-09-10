//! Linear-light adjustment math. No RGB clipping or implicit display tone curve.
use super::{RenderError, WhiteBalance};
#[repr(C)]
struct WhitePoint { x: f64, y: f64 }
unsafe extern "C" { fn pc_temperature_xy(kelvin: f64, tint: f64, out: *mut WhitePoint) -> i32; }
pub fn white_point(wb: &WhiteBalance) -> Result<Option<[f64;2]>, RenderError> {
    match wb {
        WhiteBalance::AsShot => Ok(None),
        WhiteBalance::TemperatureTint { kelvin, tint } => {
            let mut out=WhitePoint { x:0.0,y:0.0 };
            if unsafe { pc_temperature_xy(f64::from(*kelvin),f64::from(*tint),&mut out) }!=0 {
                return Err(RenderError::InvalidInput("invalid temperature/tint white point".into()));
            }
            Ok(Some([out.x,out.y]))
        }
    }
}
type Matrix=[[f64;3];3];
const RGB_XYZ:Matrix=[[0.4124564,0.3575761,0.1804375],[0.2126729,0.7151522,0.0721750],[0.0193339,0.1191920,0.9503041]];
const XYZ_RGB:Matrix=[[3.2404542,-1.5371385,-0.4985314],[-0.9692660,1.8760108,0.0415560],[0.0556434,-0.2040259,1.0572252]];
const BRADFORD:Matrix=[[0.8951,0.2664,-0.1614],[-0.7502,1.7135,0.0367],[0.0389,-0.0685,1.0296]];
const BRADFORD_INV:Matrix=[[0.9869929,-0.1470543,0.1599627],[0.4323053,0.5183603,0.0492912],[-0.0085287,0.0400428,0.9684867]];
fn mul(a:Matrix,b:Matrix)->Matrix { let mut out=[[0.0;3];3];for i in 0..3 { for j in 0..3 { for k in 0..3 { out[i][j]+=a[i][k]*b[k][j]; } } }out }
fn vector(a:Matrix,v:[f64;3])->[f64;3] { a.map(|r|r[0]*v[0]+r[1]*v[1]+r[2]*v[2]) }
/// D65 is the raster reference. Treat selected xy as the source illuminant and
/// adapt it to D65; this is a declared correction, not inferred camera metadata.
pub(crate) fn adapt_white(pixels:&mut [[f32;4]],xy:[f64;2]) {
    let src=vector(BRADFORD,[xy[0]/xy[1],1.0,(1.0-xy[0]-xy[1])/xy[1]]);
    let dst=vector(BRADFORD,[0.3127/0.3290,1.0,(1.0-0.3127-0.3290)/0.3290]);
    let mut diagonal=[[0.0;3];3];for i in 0..3 { diagonal[i][i]=dst[i]/src[i]; }
    let m=mul(XYZ_RGB,mul(BRADFORD_INV,mul(diagonal,mul(BRADFORD,RGB_XYZ))));
    for p in pixels { let rgb=vector(m,[p[0] as f64,p[1] as f64,p[2] as f64]);for i in 0..3 {p[i]=rgb[i] as f32;} }
}
pub(crate) fn luma(p:&[f32;4])->f32 { 0.2126*p[0]+0.7152*p[1]+0.0722*p[2] }

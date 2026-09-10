"""Independent float64 specification oracle for the fixed V1 qualification cases.

Uses direct neighborhood/convolution sums rather than the production sliding sums.
No production decoder, editor, LCMS or output encoder is called here. Scientific
libraries are imported lazily so registry/ownership contracts need only stdlib.
"""
from __future__ import annotations
import math
import struct

# Proposed before measurements. Source RGB magnitude <=4; small fixture dimensions
# <=32. 2e-5 absolute covers matrix quantization, <=64 float32 accumulation ulps;
# 2e-5 relative covers nonlinear rounding propagation. No observed-error fitting.
ABS_TOL = 2e-5
REL_TOL = 2e-5
GEOMETRY_ABS_TOL = 4e-5
LUMA = (.2126, .7152, .0722)
RGB_XYZ = ((.4124564,.3575761,.1804375),(.2126729,.7151522,.0721750),(.0193339,.1191920,.9503041))
BRADFORD = ((.8951,.2664,-.1614),(-.7502,1.7135,.0367),(.0389,-.0685,1.0296))
# The two Robertson brackets needed by the predeclared 4300 K and 6500 K cases.
# Wyszecki & Stiles Color Science 2nd ed p228; same published data used by DNG SDK.
ROBERTSON = {4300: ((225,.21807,.32909,-1.2168),(250,.22511,.33439,-1.4512)),
             6500: ((150,.19962,.30921,-.70471),(175,.20525,.31647,-.84901))}


def np_module():
    import numpy as np
    return np


def selected_xy(kelvin, tint):
    a, b = ROBERTSON[kelvin]  # Unknown temperatures require another frozen oracle.
    f = (1e6 / kelvin - a[0]) / (b[0] - a[0])
    u, v = (a[i]*(1-f) + b[i]*f for i in (1,2))
    direction = [(1-f)*a_i/math.hypot(1,a[3]) + f*b_i/math.hypot(1,b[3])
                 for a_i,b_i in ((1,1),(a[3],b[3]))]
    norm = math.hypot(*direction)
    u -= tint*direction[0]/(3000*norm)
    v -= tint*direction[1]/(3000*norm)
    return 1.5*u/(u-4*v+2), v/(u-4*v+2)


def white_balance(pixels, wb):
    np = np_module()
    out = np.array(pixels, dtype=np.float64, copy=True)
    if wb['mode'] == 'as_shot':
        return out
    x,y = selected_xy(wb['kelvin'], wb['tint'])
    basis = np.asarray(BRADFORD)
    rgb = np.asarray(RGB_XYZ)
    source = basis @ np.array([x/y, 1, (1-x-y)/y])
    target = basis @ np.array([.3127/.3290,1,(1-.3127-.3290)/.3290])
    # Solve linear systems independently instead of copying rounded inverse tables.
    matrix = np.linalg.solve(rgb, np.linalg.solve(basis, np.diag(target/source) @ basis @ rgb))
    out[...,:3] = out[...,:3] @ matrix.T
    return out


def denoise(pixels, n):
    np = np_module()
    if not (n['luminance'] or n['chroma']):
        return pixels.copy()
    src, out = pixels, pixels.copy()
    h,w = src.shape[:2]
    for y in range(h):
        for x in range(w):
            p = src[y,x]
            if p[3] == 0:
                continue
            neighborhood = np.array([src[min(h-1,max(0,y+dy)),min(w-1,max(0,x+dx))]
                                     for dy in (-1,0,1) for dx in (-1,0,1)])
            level = p[:3] @ LUMA
            tolerance = .03 + .25*(abs(level)+.18)*max(n.values())
            weights = neighborhood[:,3]/(1+((neighborhood[:,:3] @ LUMA-level)/tolerance)**2)
            if weights.sum() == 0:
                continue
            mean = np.sum(neighborhood[:,:3]*weights[:,None], axis=0)/weights.sum()
            mean_y = mean @ LUMA
            output_y = level+(mean_y-level)*n['luminance']
            out[y,x,:3] = output_y+(p[:3]-level)+((mean-mean_y)-(p[:3]-level))*n['chroma']
    return out


def tone_color(p, r):
    np = np_module()
    out = p.copy()
    rgb = out[...,:3] * 2**r['exposure_ev']
    magnitude = np.abs(rgb @ LUMA)
    gain = 2**(2*(r['shadows']/(1+8*magnitude)**2 + r['highlights']*magnitude/(magnitude+.5)))
    gain *= ((magnitude+.18)/.36)**(r['contrast']*.5)
    rgb *= gain[...,None]
    y = rgb @ LUMA
    hi,lo = rgb.max(axis=-1),rgb.min(axis=-1)
    chroma = np.clip((hi-lo)/(np.abs(hi)+np.abs(lo)+.18),0,1)
    saturation = (1+r['saturation'])*(1+r['vibrance']*(1-chroma))
    out[...,:3] = y[...,None]+(rgb-y[...,None])*saturation[...,None]
    return out


def weighted_straight(points, weights):
    np = np_module()
    p = np.asarray(points)
    wa = p[:,3]*np.asarray(weights)
    a = wa.sum()
    if a <= 0:
        return np.zeros(4)
    return np.r_[np.sum(p[:,:3]*wa[:,None],axis=0)/a, min(1,max(0,a))]


def geometry(pixels, recipe):
    np = np_module()
    h,w = pixels.shape[:2]
    c = recipe['crop'] or dict(left=0,top=0,right=1,bottom=1)
    # Recipe fields are f32; use those exact stored values for integer crop edges.
    l,t,r,b = [math.floor(float(np.float32(c[k]))*size+.5)
               for k,size in [('left',w),('top',h),('right',w),('bottom',h)]]
    if r<=l or b<=t:
        raise ValueError('collapsed reference crop')
    if recipe['straighten_degrees'] == 0:
        return pixels[t:b,l:r].copy()
    theta = math.radians(float(np.float32(recipe['straighten_degrees'])))
    rotation = np.array([[math.cos(theta),math.sin(theta)],[-math.sin(theta),math.cos(theta)]])
    center = np.array([(w-1)/2,(h-1)/2])
    out = np.zeros((b-t,r-l,4))
    for y in range(b-t):
        for x in range(r-l):
            xx,yy = rotation @ (np.array([x+l,y+t])-center)+center
            ix,iy = math.floor(xx),math.floor(yy)
            points,weights = [],[]
            for dy in (0,1):
                for dx in (0,1):
                    points.append(pixels[iy+dy,ix+dx] if 0<=iy+dy<h and 0<=ix+dx<w else np.zeros(4))
                    weights.append((xx-ix if dx else 1-xx+ix)*(yy-iy if dy else 1-yy+iy))
            out[y,x] = weighted_straight(points,weights)
    return out


def sharpen(pixels, spec):
    np = np_module()
    if not spec['amount']:
        return pixels.copy()
    h,w = pixels.shape[:2]
    radius = math.ceil(spec['radius_px'])
    edge = spec['radius_px']-(radius-1)
    offsets = list(range(-radius,radius+1))
    weights = [edge if abs(v)==radius else 1 for v in offsets]
    out = pixels.copy()
    # Direct 2D separable-kernel product: independent of Rust's two rolling sums.
    for y in range(h):
        for x in range(w):
            points,weight = [],[]
            for dy,wy in zip(offsets,weights):
                for dx,wx in zip(offsets,weights):
                    points.append(pixels[min(h-1,max(0,y+dy)),min(w-1,max(0,x+dx))])
                    weight.append(wx*wy)
            p = np.asarray(points)
            wa = p[:,3]*weight
            mean = np.sum(p[:,:3]*wa[:,None],axis=0)/wa.sum() if wa.sum()>0 else pixels[y,x,:3]
            out[y,x,:3] += spec['amount']*(pixels[y,x,:3]-mean)
    return out


def render(pixels, recipe, include_wb=True):
    r = recipe['settings']
    p = white_balance(pixels,r['white_balance']) if include_wb else pixels.copy()
    p = denoise(p,r['noise_reduction'])
    p = tone_color(p,r)
    p = geometry(p,r)
    return sharpen(p,r['sharpening'])


def compare(actual, expected, *, geometry_changed=False):
    np = np_module()
    if actual.shape != expected.shape or not np.isfinite(actual).all():
        raise ValueError('shape/nonfinite oracle failure')
    absolute = GEOMETRY_ABS_TOL if geometry_changed else ABS_TOL
    error = np.abs(actual-expected)
    allowed = absolute+REL_TOL*np.abs(expected)
    failed = error>allowed
    return dict(pass_=not bool(failed.any()), failed_components=int(failed.sum()),
                components=int(error.size), max_abs=float(error.max()),
                abs_tolerance=absolute,relative_tolerance=REL_TOL)


def matrix_profile(gamma=1.0):
    """ICC v2 matrix RGB, known D50 columns and analytic gamma, no LCMS generator."""
    def fixed(x): return struct.pack('>i',round(x*65536))
    def xyz(v): return b'XYZ '+b'\0'*4+b''.join(map(fixed,v))
    columns = ((.4360747,.2225045,.0139322),(.3850649,.7168786,.0971045),(.1430804,.0606169,.7141733))
    desc = b'PhotoCatalog analytical sRGB primaries\0'
    tags = {b'desc':b'desc'+b'\0'*4+struct.pack('>I',len(desc))+desc+b'\0'*78,
            b'cprt':b'text'+b'\0'*4+b'Generated numerical fixture; CC0\0',
            b'wtpt':xyz((.9642,1,.8249))}
    for channel,vector in zip(b'rgb',columns):
        tags[bytes([channel])+b'XYZ'] = xyz(vector)
        tags[bytes([channel])+b'TRC'] = b'curv'+b'\0'*4+struct.pack('>IH',1,round(gamma*256))+b'\0\0'
    header = bytearray(128)
    header[8:24] = b'\x02\x10\0\0mntrRGB XYZ '
    header[24:36] = struct.pack('>6H',2000,1,1,0,0,0)
    header[36:40] = b'acsp'
    header[64:68] = struct.pack('>I',1)
    header[68:80] = b''.join(map(fixed,(.9642,1,.8249)))
    table = bytearray(struct.pack('>I',len(tags)))
    body = bytearray()
    start = 128+4+len(tags)*12
    for signature,payload in sorted(tags.items()):
        table += signature+struct.pack('>II',start+len(body),len(payload))
        body += payload+b'\0'*((-len(payload))%4)
    encoded = header+table+body
    encoded[:4] = struct.pack('>I',len(encoded))
    return bytes(encoded)


def srgb_matrix_d50():
    """Derive chromaticity matrix and Bradford adaptation, rather than call LCMS."""
    np=np_module()
    primaries=np.array([[.64/.33,.3/.6,.15/.06],[1,1,1],[(1-.64-.33)/.33,(1-.3-.6)/.6,(1-.15-.06)/.06]])
    d65=np.array([.3127/.329,1,(1-.3127-.329)/.329])
    matrix=primaries @ np.diag(np.linalg.solve(primaries,d65))
    b=np.array(BRADFORD)
    d50=np.array([.9642,1,.8249])
    return np.linalg.solve(b,np.diag((b@d50)/(b@d65))@b@matrix)


def fixture_to_linear(pixels):
    np=np_module()
    # Generated ICC XYZ tags are signed 15.16 fixed point; use their exact bytes.
    matrix=np.array([[.4360747,.3850649,.1430804],[.2225045,.7168786,.0606169],[.0139322,.0971045,.7141733]])
    matrix=np.rint(matrix*65536)/65536
    out=np.array(pixels,dtype=np.float64,copy=True)
    out[...,:3]=out[...,:3] @ np.linalg.solve(srgb_matrix_d50(),matrix).T
    return out


def resize(pixels, width, height):
    np=np_module()
    h,w=pixels.shape[:2]
    if (w,h)==(width,height):
        return pixels.copy()
    out=np.zeros((height,width,4))
    sx,sy=w/width,h/height
    for y in range(height):
        for x in range(width):
            points,weights=[],[]
            if sx>=1 and sy>=1:
                left,right,top,bottom=x*sx,(x+1)*sx,y*sy,(y+1)*sy
                for yy in range(math.floor(top),min(h,math.ceil(bottom))):
                    for xx in range(math.floor(left),min(w,math.ceil(right))):
                        points.append(pixels[yy,xx])
                        weights.append((min(xx+1,right)-max(xx,left))*(min(yy+1,bottom)-max(yy,top))/(sx*sy))
            else:
                xx,yy=(x+.5)*sx-.5,(y+.5)*sy-.5
                ix,iy=math.floor(xx),math.floor(yy)
                for dy in (0,1):
                    for dx in (0,1):
                        points.append(pixels[min(h-1,max(0,iy+dy)),min(w-1,max(0,ix+dx))])
                        weights.append((xx-ix if dx else 1-xx+ix)*(yy-iy if dy else 1-yy+iy))
            out[y,x]=weighted_straight(points,weights)
    return out


def output_pixels(pixels, spec):
    np=np_module()
    result=pixels.copy()
    size=spec['size']
    if size['mode']=='fit':
        h,w=result.shape[:2]
        factor=min(size['width']/w,size['height']/h)
        if not size['allow_upscale']:
            factor=min(1,factor)
        result=resize(result,max(1,math.floor(w*factor+.5)),max(1,math.floor(h*factor+.5)))
    if spec['alpha']['mode']=='composite':
        result[...,:3]=result[...,:3]*result[...,3,None]+np.asarray(spec['alpha']['linear_rgb'])*(1-result[...,3,None])
        result[...,3]=1
    kind=spec['profile']['kind']
    if kind=='srgb':
        rgb=result[...,:3]
        # np.where evaluates both branches: use mask to avoid invalid negative powers.
        positive=rgb>.0031308
        rgb[~positive]*=12.92
        rgb[positive]=1.055*rgb[positive]**(1/2.4)-.055
    elif kind=='icc':
        profile=bytes(spec['profile']['bytes'])
        gamma=1 if profile==matrix_profile(1) else 2 if profile==matrix_profile(2) else None
        if gamma is None:
            raise ValueError('custom ICC is not the frozen analytic profile')
        target=np.array([[.4360747,.3850649,.1430804],[.2225045,.7168786,.0606169],[.0139322,.0971045,.7141733]])
        target=np.rint(target*65536)/65536
        result[...,:3]=result[...,:3] @ np.linalg.solve(target,srgb_matrix_d50()).T
        if gamma!=1:
            # ICC power curve has domain >=0. Custom nonlinear integer fixtures
            # are restricted to positive interior patches, no invented HDR curve.
            if (result[...,:3]<0).any():
                raise ValueError('negative custom nonlinear oracle input')
            result[...,:3] **= 1/gamma
    fmt=spec['format']
    if fmt.get('depth')=='float32':
        return result
    maximum=65535 if fmt.get('depth')=='sixteen' else 255
    quantized=np.floor(np.clip(result,0,1)*maximum+.5)
    if fmt['format']=='jpeg' or spec['alpha']['mode']=='composite':
        quantized=quantized[...,:3]
    return quantized.astype(np.uint16 if maximum==65535 else np.uint8)

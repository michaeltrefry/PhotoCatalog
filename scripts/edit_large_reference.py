"""Bounded independent reference for the generated 100-MP capability fixture.

Neutral checks every component in row blocks. Combined checks the declared fixed
17x17 output grid, including boundaries, by evaluating required source neighborhoods
and the exact inverse geometry in float64. No product code/probe result supplies
expected pixels. Small-fixture tests compare this evaluator to the full oracle.
"""
from functools import lru_cache
import math
import edit_reference as ref


def expected_dimensions(width,height,recipe):
    np=ref.np_module()
    c=recipe['settings']['crop'] or dict(left=0,top=0,right=1,bottom=1)
    l,t,r,b=[math.floor(float(np.float32(c[k]))*size+.5)
             for k,size in [('left',width),('top',height),('right',width),('bottom',height)]]
    return l,t,r-l,b-t


def generated_source(x,y):
    # Independent scalar form of the declared periodic binary-fraction image.
    return [((x+3*y)%17-2)/8,((3*x+y)%13-1)/8,((5*x+7*y)%19-3)/8,[0,.25,.5,1][(x+y)%4]]


def point_evaluator(width,height,recipe):
    np=ref.np_module()
    settings=recipe['settings']
    left,top,out_w,out_h=expected_dimensions(width,height,recipe)
    theta=math.radians(float(np.float32(settings['straighten_degrees'])))
    c,s=math.cos(theta),math.sin(theta)
    center_x,center_y=(width-1)/2,(height-1)/2

    @lru_cache(maxsize=8192)
    def developed(x,y):
        if not(0<=x<width and 0<=y<height):return np.zeros(4)
        neighborhood=np.array([[generated_source(min(width-1,max(0,x+dx)),min(height-1,max(0,y+dy)))
                                for dx in (-1,0,1)] for dy in (-1,0,1)],dtype=np.float64)
        prepared=ref.white_balance(ref.fixture_to_linear(neighborhood),settings['white_balance'])
        denoised=ref.denoise(prepared,settings['noise_reduction'])
        return ref.tone_color(denoised[1:2,1:2],settings)[0,0]

    @lru_cache(maxsize=8192)
    def geometric(x,y):
        if not settings['straighten_degrees']:
            return developed(x+left,y+top)
        a,b=x+left-center_x,y+top-center_y
        xx,yy=c*a+s*b+center_x,-s*a+c*b+center_y
        ix,iy=math.floor(xx),math.floor(yy)
        points,weights=[],[]
        for dy in (0,1):
            for dx in (0,1):
                points.append(developed(ix+dx,iy+dy))
                weights.append((xx-ix if dx else 1-xx+ix)*(yy-iy if dy else 1-yy+iy))
        return ref.weighted_straight(points,weights)

    def evaluate(x,y):
        if not(0<=x<out_w and 0<=y<out_h):raise ValueError('point outside output')
        result=geometric(x,y).copy()
        sharpening=settings['sharpening']
        if sharpening['amount']:
            radius=math.ceil(sharpening['radius_px'])
            edge=sharpening['radius_px']-(radius-1)
            points,weights=[],[]
            for dy in range(-radius,radius+1):
                for dx in range(-radius,radius+1):
                    points.append(geometric(min(out_w-1,max(0,x+dx)),min(out_h-1,max(0,y+dy))))
                    weights.append((edge if abs(dx)==radius else 1)*(edge if abs(dy)==radius else 1))
            points=np.asarray(points)
            wa=points[:,3]*weights
            mean=(points[:,:3]*wa[:,None]).sum(axis=0)/wa.sum() if wa.sum()>0 else result[:3]
            result[:3]+=sharpening['amount']*(result[:3]-mean)
        return result
    return (out_w,out_h),evaluate


def verify_large(actual,recipe,width=10000,height=10000):
    np=ref.np_module()
    settings=recipe['settings']
    (out_w,out_h),point=point_evaluator(width,height,recipe)
    if actual.shape!=(out_h,out_w,4):raise ValueError('100MP output geometry mismatch')
    neutral=(settings['white_balance']['mode']=='as_shot' and settings['crop'] is None
             and all(settings[k]==0 for k in ('straighten_degrees','exposure_ev','contrast','highlights','shadows','saturation','vibrance'))
             and settings['sharpening']['amount']==0 and all(v==0 for v in settings['noise_reduction'].values()))
    count=0
    maximum=0.
    if neutral:
        x=np.arange(width)
        for start in range(0,height,8):
            rows=min(8,height-start)
            expected=np.empty((rows,width,4),dtype=np.float64)
            for row in range(rows):
                y=start+row
                expected[row,:,0]=((x+3*y)%17-2)/8
                expected[row,:,1]=((3*x+y)%13-1)/8
                expected[row,:,2]=((5*x+7*y)%19-3)/8
                expected[row,:,3]=np.array([0,.25,.5,1])[(x+y)%4]
            proof=ref.compare(actual[start:start+rows],ref.fixture_to_linear(expected))
            if not proof['pass_']:raise ValueError('100MP full neutral stream mismatch')
            count+=proof['components']; maximum=max(maximum,proof['max_abs'])
        return dict(kind='every_neutral_component_streamed',components=count,max_abs=maximum)
    coordinates=sorted({(round(i*(out_w-1)/16),round(j*(out_h-1)/16)) for i in range(17) for j in range(17)})
    for x,y in coordinates:
        proof=ref.compare(actual[y,x][None,None,:],point(x,y)[None,None,:],geometry_changed=bool(settings['straighten_degrees']))
        if not proof['pass_']:raise ValueError('100MP combined fixed-point mismatch')
        count+=proof['components'];maximum=max(maximum,proof['max_abs'])
    return dict(kind='fixed17x17_combined_grid_including_boundaries',components=count,
                points=coordinates,max_abs=maximum,full_combined_pixel_oracle=False)

#!/usr/bin/env python3
"""Regenerate mathematical DNG/color oracle; requires numpy and tifffile. No photos."""
from pathlib import Path
import numpy as np,tifffile,json
root=Path(__file__).resolve().parents[1]/'tests/fixtures'
n=np.array([.5,1.,.75]);fm=np.array([[.5,.3,.1643],[.2,.7,.1],[.05,.1,.6751]])
cm=np.diag(n)@np.linalg.inv(fm)
# Numeric fixtures are generated mathematical swatches, not photographs.
patch=np.array([n*.25,[.2,0,0],[0,.2,0],[0,0,.2],n,[.5,0,0]],dtype=np.float32)
a=np.tile(patch,(16,6,1))
mask=np.tile(np.array([0,128,255,255,255,128],dtype=np.uint8),(16,6))
def rational(values):
 out=[]
 for v in np.array(values).ravel(): out.extend([int(round(float(v)*1000000)),1000000])
 return tuple(out)
tags=[(254,"I",1,0,False),(50706,'B',4,(1,4,0,0),False),(50707,'B',4,(1,4,0,0),False),(50708,'s',0,'PhotoCatalog mathematical fixture',False),(50721,'2i',9,rational(cm),False),(50964,'2i',9,rational(fm),False),(50778,'H',1,23,False),(50728,'2I',3,rational(n),False),(50717,'I',3,(1,1,1),False),(50714,'2I',3,(0,1,0,1,0,1),False),(50713,'H',2,(1,1),False)]
with tifffile.TiffWriter(root/'generated-linear-mask.dng') as w:
 w.write(a,photometric="rgb",planarconfig="contig",metadata=None,extratags=tags,subifds=1,rowsperstrip=16)
 w.write(mask,photometric=4,extratags=[(254,"I",1,0,False),(254,"I",1,4,False)],metadata=None,rowsperstrip=16)
with tifffile.TiffFile(root/'generated-linear-mask.dng') as source:
 offset=source.pages[0].tags[262].valueoffset
with (root/'generated-linear-mask.dng').open('r+b') as output:
 output.seek(offset);output.write((34892).to_bytes(2,'little'))
xyz_to_srgb=np.array([[3.1338561,-1.6168667,-.4906146],[-.9787684,1.9161415,.0334540],[.0719453,-.2289914,1.4052427]])
expected=(xyz_to_srgb@fm@np.diag(1/n)@patch.T).T
(root/'generated-linear-mask.expected.json').write_text(json.dumps({'linear_srgb':expected.tolist(),'alpha':[0,128/255,1,1,1,128/255],'width':36,'height':16},indent=2)+'\n')
print(expected)

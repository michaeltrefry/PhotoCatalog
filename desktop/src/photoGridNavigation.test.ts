import {describe,expect,it} from 'vitest';
import {createKeyboardFocusRequest,focusLeavesGrid} from './photoGridNavigation';

describe('photo grid keyboard focus',()=>{
  it('moves focus only after the requested asynchronous selection renders',()=>{
    let active='first';const candidate=(image:string)=>({dataset:{image},focus:()=>{active=image;}});
    const request=createKeyboardFocusRequest(),buttons=[candidate('first'),candidate('fifth')];request.request('fifth');
    expect(request.settle('first',buttons)).toBe(false);expect(active).toBe('first');
    expect(request.settle('sixth',buttons)).toBe(false);expect(active).toBe('first');
    expect(request.settle('fifth',buttons)).toBe(true);expect(active).toBe('fifth');
  });
  it('retains a keyboard request for internal blur and cancels it before inspector focus',()=>{
    const current={} as Node,inspector={} as Node,grid={contains:(node:Node|null)=>node===current};
    expect(focusLeavesGrid(grid,current)).toBe(false);expect(focusLeavesGrid(grid,inspector)).toBe(true);
    let focused=false;const request=createKeyboardFocusRequest();request.request('next');request.cancel();
    expect(request.settle('next',[{dataset:{image:'next'},focus:()=>{focused=true;}}])).toBe(false);expect(focused).toBe(false);
  });
});

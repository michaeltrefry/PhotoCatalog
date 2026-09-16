import {describe,expect,it} from 'vitest';
import {observeInputPoll} from './useLightroomInput';

describe('Lightroom staged input polling identity',()=>{
  it('suppresses a pending failure after a new guard renders but retains a current failure',async()=>{
    let rejectOld!:(error:unknown)=>void,latest='old';
    const old=observeInputPoll(new Promise<never>((_,reject)=>{rejectOld=reject;}),()=>latest==='old');
    latest='new';rejectOld(new Error('stale generation'));
    expect(await old).toEqual({current:false});
    const error=new Error('current failure');
    const current=await observeInputPoll(Promise.reject(error),()=>latest==='new');
    expect(current).toEqual({current:true,error});
  });
});

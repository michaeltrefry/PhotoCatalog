import {describe,expect,it} from 'vitest';
import {acceptInputPoll,observeInputPoll} from './useLightroomInput';

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
  it('rechecks identity when a success was current at settlement but superseded before consumption',async()=>{
    let resolve!:(value:string)=>void,latest='old';
    const pending=observeInputPoll(new Promise<string>(yes=>{resolve=yes;}),()=>latest==='old');
    resolve('old reply');
    await Promise.resolve();
    latest='new';
    const observed=await pending;
    expect(observed).toEqual({current:true,reply:'old reply'});
    expect(acceptInputPoll(observed,()=>latest==='old')).toEqual({current:false});
  });
});

import {describe,it,expect} from 'vitest';
import {parseLosslessJson,ResultAssembly} from './lightroomJson';
import type {ResultPage} from './lightroom';
const limits={bytes:10000,nodes:100,depth:20};
describe('lossless Lightroom evidence',()=>{
 it('preserves numeric lexemes and duplicate/unsafe keys in a collision-free whole AST',()=>{
  const raw=' {"__proto__":9007199254740993,"x":-9223372036854775808,"x":1.2300e+99,"fake":{"kind":"number","lexeme":"8"},"a":[true,null,"\\ud800"]} ';
  const value=parseLosslessJson(raw,limits);
  expect(value).toEqual({kind:'object',entries:[['__proto__',{kind:'number',lexeme:'9007199254740993'}],['x',{kind:'number',lexeme:'-9223372036854775808'}],['x',{kind:'number',lexeme:'1.2300e+99'}],['fake',{kind:'object',entries:[['kind',{kind:'string',value:'number'}],['lexeme',{kind:'string',value:'8'}]]}],['a',{kind:'array',items:[{kind:'boolean',value:true},{kind:'null'},{kind:'string',value:'\ud800'}]}]]});
  expect(Object.prototype).not.toHaveProperty('lexeme');
 });
 it('rejects malformed JSON and bounds decoded review structure',()=>{
  for(const raw of ['01','1e','[1,]','{"x":1,}','"\n"','true false','{"x" 1}','['])expect(()=>parseLosslessJson(raw,limits)).toThrow();
  expect(()=>parseLosslessJson('"😀"',{...limits,bytes:5})).toThrow();
  expect(()=>parseLosslessJson('[1,2]',{...limits,nodes:2})).toThrow();
  expect(()=>parseLosslessJson('[[1]]',{...limits,depth:1})).toThrow();
 });
 it('reassembles exact bytes including approval whitespace without numeric interpretation',()=>{
  const id={attempt:'a',workbench:'w',generation:'g',operation:'o',token:'t'};
  const raw=' \n{"a":18446744073709551615,"p":"😀"} \t';const cuts=[' \n{"a":18446744073709551615,','"p":"😀"} \t'];
  const assembly=new ResultAssembly(id,1000n);let offset=0;
  for(const [n,json_fragment] of cuts.entries()){const next=offset+new TextEncoder().encode(json_fragment).length;assembly.append({...id,offset:String(offset),next:n===0?String(next):null,total_bytes:String(new TextEncoder().encode(raw).length),json_fragment});offset=next;}
  expect(assembly.finish()).toBe(raw);
 });
 it('rejects stale tokens, changed totals, duplicate chunks, wrong byte cursors and incomplete results',()=>{
  const id={attempt:'a',workbench:'w',generation:'g',operation:'o',token:'t'};const p:ResultPage={...id,offset:'0',next:'1',total_bytes:'2',json_fragment:'1'};
  const a=new ResultAssembly(id,2n);expect(()=>a.finish()).toThrow();for(const key of ['attempt','workbench','generation','operation','token'] as const)expect(()=>a.append({...p,[key]:'stale'})).toThrow();
  expect(()=>a.append({...p,total_bytes:'3'})).toThrow();expect(()=>a.append({...p,offset:'00'})).toThrow();a.append(p);expect(()=>a.append(p)).toThrow();expect(()=>a.append({...p,offset:'1',next:null,total_bytes:'3'})).toThrow();a.append({...p,offset:'1',next:null});expect(a.finish()).toBe('11');expect(()=>a.append(p)).toThrow();
 });
});

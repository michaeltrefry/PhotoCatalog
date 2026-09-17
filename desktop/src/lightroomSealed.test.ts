import {describe,expect,it,vi} from 'vitest';
import {loadSealedBundle,type RetainedRead} from './lightroomSealed';
import type {NativePath} from './bridge';
import type {SealedDocumentKind,SealedDocumentPage,SealedDocumentRequest} from './lightroom';
import {blake3} from '@noble/hashes/blake3.js';
import {bytesToHex} from '@noble/hashes/utils.js';

const path=(text:string):NativePath=>({encoding:'UnixBytes',units:[...new TextEncoder().encode(text)]});
const digest=(bytes:Uint8Array)=>bytesToHex(blake3(bytes));
function transport(documents:Record<SealedDocumentKind,string>){
  let retained:null|{session:string;document:SealedDocumentKind;bytes:Uint8Array}=null;
  return vi.fn(async(request:SealedDocumentRequest):Promise<SealedDocumentPage|null>=>{
    if(request.action==='begin'){
      if(retained&&retained.session!==request.session)throw new Error('busy');
      retained??={session:request.session,document:request.document,bytes:new TextEncoder().encode(documents[request.document])};
      return page(retained,0,0);
    }
    if(request.action==='discard'){if(!retained||retained.session!==request.session)throw new Error('wrong session');retained=null;return null;}
    if(!retained||retained.session!==request.session)throw new Error('wrong session');
    return page(retained,Number(request.offset),Number(request.limit));
  });
}
function page(value:{session:string;document:SealedDocumentKind;bytes:Uint8Array},offset:number,limit:number):SealedDocumentPage{
  const end=Math.min(offset+limit,value.bytes.length);
  return {session:value.session,directory:path('/sealed'),path:path(`/sealed/${value.document}.json`),document:value.document,physical:{kind:'fixture'},total_bytes:String(value.bytes.length),blake3:digest(value.bytes),offset:String(offset),next:end<value.bytes.length?String(end):null,bytes:[...value.bytes.slice(offset,end)]};
}
describe('sealed Lightroom document transfer',()=>{
  it('retains exact UTF-8 bytes, checks the seal binding, derives policy losslessly, and discards each F snapshot',async()=>{
    const approval='{"protocol":1,"destination":{"encoding":"UnixBytes","units":[47,100,115,116]},"policy":{"limit":9007199254740993,"supplements":[]}}\n';
    const approvalHash=digest(new TextEncoder().encode(approval));
    const seal=`{"approval":{"document_blake3":"${approvalHash}"}}\n`;
    const send=transport({seal,approval}),states:RetainedRead[]=[];
    const result=await loadSealedBundle(path('/chosen'),undefined,value=>{if(value)states.push(value);},send);
    expect(result.seal.text).toBe(seal);expect(result.approval.text).toBe(approval);
    expect(result.policy.text).toBe('{"limit":9007199254740993,"supplements":[]}');
    expect(result.destination.display).toBe('/dst');
    expect(states.map(value=>value.document)).toEqual(['seal','approval']);
    expect(send.mock.calls.filter(([request])=>request.action==='discard')).toHaveLength(2);
  });
  it('rejects a seal whose exact approval digest differs',async()=>{
    const approval='{"destination":{"encoding":"UnixBytes","units":[47]},"policy":{}}';
    await expect(loadSealedBundle(path('/chosen'),undefined,undefined,transport({seal:'{"approval":{"document_blake3":"0000000000000000000000000000000000000000000000000000000000000000"}}',approval}))).rejects.toThrow('differs');
  });
  it('preserves a UTF-8 BOM through transfer before rejecting unsupported JSON bytes',async()=>{
    const approval='\ufeff{"destination":{"encoding":"UnixBytes","units":[47]},"policy":{}}';
    const approvalHash=digest(new TextEncoder().encode(approval));
    await expect(loadSealedBundle(path('/chosen'),undefined,undefined,transport({seal:`{"approval":{"document_blake3":"${approvalHash}"}}`,approval}))).rejects.toThrow('Invalid JSON');
  });
});

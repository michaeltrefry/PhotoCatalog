import type { NativePath } from './bridge';
import type { Guard, InputStatus, Query } from './lightroom';
import { type JsonNode } from './lightroomJson';

export function decimal(value:string, name:string, minimum=0n, maximum=18446744073709551615n):string {
  if(!/^(0|[1-9][0-9]*)$/.test(value)||BigInt(value)<minimum||BigInt(value)>maximum)throw new Error(`${name} must be a whole decimal from ${minimum} to ${maximum}.`);
  return value;
}
export function limits<T extends object>(value:T):T { for(const [key,item] of Object.entries(value))decimal(item as string,key.replaceAll('_',' '),1n);return {...value}; }
export const guardKey=(g:Guard)=>JSON.stringify([g.workbench,g.generation,g.operation]);
export function encodeInput(text:string,maximum=16777216):Uint8Array {
  if(!Number.isSafeInteger(maximum)||maximum<0||maximum>16777216||text.length>maximum)throw new Error('Document exceeds the input byte allowance.');
  for(const point of text){const code=point.codePointAt(0)!;if(code>=0xd800&&code<=0xdfff)throw new Error('Document contains an unpaired UTF-16 surrogate; exact UTF-8 cannot be preserved.');}
  const bytes=new TextEncoder().encode(text);if(bytes.length>maximum)throw new Error('Document exceeds the input byte allowance.');return bytes;
}
export const utf8Length=(text:string)=>encodeInput(text).length;
/** The immutable document is encoded once. Each chunk examines at most three
 * boundary bytes, then decodes only its own bounded view. Reconciliation chooses
 * the observed byte offset; a locally queued append never advances it. */
export function inputChunk(bytes:Uint8Array,offset:string,maximum:string):{fragment:string;next:string} {
  const startValue=BigInt(decimal(offset,'Input offset')),max=Number(decimal(maximum,'Chunk bytes',4n,131072n));
  if(startValue>BigInt(bytes.length))throw new Error('Input offset exceeds the document.');
  const start=Number(startValue);if(start<bytes.length&&(bytes[start]&0xc0)===0x80)throw new Error('Input offset splits a Unicode scalar.');
  let end=Math.min(start+max,bytes.length);while(end<bytes.length&&(bytes[end]&0xc0)===0x80)end--;
  return {fragment:new TextDecoder('utf-8',{fatal:true,ignoreBOM:true}).decode(bytes.subarray(start,end)),next:String(end)};
}
export function field(node:JsonNode,key:string):JsonNode|undefined {
  if(node.kind!=='object')return;const found=node.entries.filter(([name])=>name===key);if(found.length>1)throw new Error(`Ambiguous duplicate field ${key}; inspect the retained JSON.`);return found[0]?.[1];
}
export function scalar(node:JsonNode|undefined):string { if(node?.kind==='string')return node.value;if(node?.kind==='number')return node.lexeme;throw new Error('Expected an exact string or numeric value.'); }
export const array=(node:JsonNode|undefined):JsonNode[]=>node?.kind==='array'?node.items:[];
export function nativePath(node:JsonNode):NativePath {
  const encoding=scalar(field(node,'encoding'));if(encoding!=='UnixBytes'&&encoding!=='WindowsWide')throw new Error('Unknown native path encoding.');
  const values=field(node,'units');if(values?.kind!=='array'||!values.items.length||values.items.length>1048576)throw new Error('Invalid native path length.');
  return {encoding,units:values.items.map(value=>Number(decimal(scalar(value),'Native path unit',1n,encoding==='UnixBytes'?255n:65535n)))};
}
export type Family={id:string;evidence:string;members:{revision:string;source:JsonNode;details:JsonNode}[];details:JsonNode};
export function families(node:JsonNode):Family[] {return array(field(node,'families')).map(value=>({id:scalar(field(value,'id')),evidence:scalar(field(value,'evidence_digest')),members:array(field(value,'members')).map(member=>({revision:scalar(field(member,'revision_id')),source:field(member,'source')!,details:member})),details:value}));}
export function selectionDocument(root:NativePath,rows:Family[],decisions:Record<string,string>):string {
  if(!rows.length)throw new Error('Read the complete family report first.');
  return JSON.stringify({inspection:root,families:rows.map(f=>{const decision=decisions[f.id];if(!decision)throw new Error(`Explicitly select or exclude family ${f.id}.`);if(decision==='exclude')return {kind:'Exclude',family:f.id,expected_evidence_digest:f.evidence};if(!f.members.some(m=>m.revision===decision))throw new Error('Selected revision is no longer a member of its family.');return {kind:'Select',family:f.id,revision:decision,expected_evidence_digest:f.evidence};})});
}
/** A short or empty sparse page is not exhaustion when the server supplies a cursor. */
export function nextQuery(query:Query,node:JsonNode):Query|null {
  if(query.kind==='PacketBytes') {const offset=BigInt(decimal(scalar(field(node,'offset')),'Packet offset')),total=BigInt(decimal(scalar(field(node,'total_bytes')),'Packet total'));const next=offset+BigInt(query.limit);return next<total?{...query,offset:next.toString()}:null;}
  const next=field(node,'next');if(!next||next.kind==='null')return null;
  if(query.kind==='GlobalIdConflicts')return {...query,after_left:scalar(field(next,'left')),after_right:scalar(field(next,'right'))};
  if(query.kind==='PathCollisions') {if(next.kind!=='array'||next.items.length!==2)throw new Error('Invalid paired continuation.');return {...query,after_left:scalar(next.items[0]),after_right:scalar(next.items[1])};}
  if('after' in query)return {...query,after:scalar(next)};
  return null;
}
export type InputExpectation={kind:'begin';purpose:InputStatus['purpose'];total:string;digest:string|null}|{kind:'append';input:string;next:string}|{kind:'finish'|'discard';input:string};
export function inputObserved(expected:InputExpectation,status:InputStatus|null):boolean {
  if(expected.kind==='discard')return status===null;
  if(!status)return false;
  if(expected.kind==='begin')return status.purpose===expected.purpose&&status.total_bytes===expected.total&&status.expected_blake3===expected.digest;
  if(status.input!==expected.input)return false;
  return expected.kind==='append'?status.received_bytes===expected.next:status.complete&&status.blake3!==null;
}

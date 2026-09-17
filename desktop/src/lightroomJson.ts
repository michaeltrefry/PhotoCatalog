import type {ResultPage,Guard} from './lightroom';
/** Every node is tagged, including source objects. Numeric lexemes cannot collide
 * with a source object. Entries preserve duplicate and unsafe keys in order.
 * This AST is a display aid; immutable raw text remains the authority. */
export type JsonNode=
 | {kind:'null'} | {kind:'boolean';value:boolean} | {kind:'string';value:string}
 | {kind:'number';lexeme:string} | {kind:'array';items:JsonNode[]}
 | {kind:'object';entries:[string,JsonNode][]};
export function parseLosslessJson(raw:string,limits:{bytes:number;nodes:number;depth:number}):JsonNode {
  if(!Number.isSafeInteger(limits.bytes)||!Number.isSafeInteger(limits.nodes)||!Number.isSafeInteger(limits.depth)||limits.bytes<1||limits.nodes<1||limits.depth<1)throw new Error('Invalid JSON review limits');
  // Reject on UTF-16 lower bound before allocating an encoding buffer.
  if(raw.length>limits.bytes||new TextEncoder().encode(raw).length>limits.bytes)throw new Error('JSON review byte limit');
  let at=0,nodes=0;
  const white=()=>{while(/[\x20\t\r\n]/.test(raw[at]??'!'))at++;};
  const string=()=>{
    if(raw[at]!=='"')throw new Error('Expected JSON string');
    const start=at++;
    while(at<raw.length){const c=raw[at++];if(c==='"')return JSON.parse(raw.slice(start,at)) as string;if(c==='\\')at++;}
    throw new Error('Unterminated JSON string');
  };
  const read=(depth:number):JsonNode=>{
    if(++nodes>limits.nodes||depth>limits.depth)throw new Error('JSON review structure limit');
    white();const c=raw[at];
    if(c==='"')return {kind:'string',value:string()};
    if(c==='['){at++;white();const items:JsonNode[]=[];if(raw[at]===']'){at++;return {kind:'array',items};}for(;;){items.push(read(depth+1));white();if(raw[at++]===']')break;if(raw[at-1]!==',')throw new Error('Expected array delimiter');}return {kind:'array',items};}
    if(c==='{'){at++;white();const entries:[string,JsonNode][]=[];if(raw[at]==='}'){at++;return {kind:'object',entries};}for(;;){white();const key=string();white();if(raw[at++]!==':')throw new Error('Expected object colon');entries.push([key,read(depth+1)]);white();if(raw[at++]==='}')break;if(raw[at-1]!==',')throw new Error('Expected object delimiter');}return {kind:'object',entries};}
    for(const [word,value] of [['true',true],['false',false],['null',null]] as const)if(raw.startsWith(word,at)){at+=word.length;return value===null?{kind:'null'}:{kind:'boolean',value};}
    const number=/^-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?/.exec(raw.slice(at));
    if(!number)throw new Error('Invalid JSON value');at+=number[0].length;return {kind:'number',lexeme:number[0]};
  };
  const value=read(0);white();if(at!==raw.length)throw new Error('Trailing JSON bytes');return value;
}
export function stringifyLosslessJson(node:JsonNode):string {
  switch(node.kind){
    case 'null':return 'null';
    case 'boolean':return node.value?'true':'false';
    case 'string':return JSON.stringify(node.value);
    case 'number':return node.lexeme;
    case 'array':return `[${node.items.map(stringifyLosslessJson).join(',')}]`;
    case 'object':return `{${node.entries.map(([key,value])=>`${JSON.stringify(key)}:${stringifyLosslessJson(value)}`).join(',')}}`;
  }
}
function unsigned(text:string):bigint {if(!/^(0|[1-9][0-9]*)$/.test(text))throw new Error('Invalid decimal cursor');return BigInt(text);}
export class ResultAssembly {
  private fragments:string[]=[];private offset=0n;private total:bigint|null=null;private done=false;
  private readonly identity:Guard & {attempt:string;token:string};
  private readonly maxBytes:bigint;
  constructor(identity:Guard & {attempt:string;token:string},maxBytes:bigint) {this.identity={...identity};this.maxBytes=maxBytes;if(maxBytes<0n)throw new Error('Invalid result byte allowance');}
  append(page:ResultPage):void {
    if(this.done)throw new Error('Result already complete');
    for(const key of ['attempt','workbench','generation','operation','token'] as const)if(page[key]!==this.identity[key])throw new Error('Stale result identity');
    const offset=unsigned(page.offset),total=unsigned(page.total_bytes);
    if(total>this.maxBytes||offset!==this.offset||(this.total!==null&&this.total!==total))throw new Error('Result byte cursor or allowance differs');
    const bytes=BigInt(new TextEncoder().encode(page.json_fragment).length),end=offset+bytes;
    if(end>total || (page.next!==null && (unsigned(page.next)!==end||end===offset||end>=total)) || (page.next===null&&end!==total))throw new Error('Invalid result continuation');
    this.fragments.push(page.json_fragment);this.offset=end;this.total=total;this.done=page.next===null;
  }
  finish():string {if(!this.done)throw new Error('Result is incomplete');return this.fragments.join('');}
}

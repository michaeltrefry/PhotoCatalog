import {useEffect,useRef,useState} from 'react';
import {errorText} from '../bridge';
import {lightroom,type Options,type ResultPage,type Status} from '../lightroom';
import {parseLosslessJson,ResultAssembly,type JsonNode} from '../lightroomJson';
import {decimal,guardKey} from '../lightroomWorkflow';
import {ErrorNotice} from './Controls';

/** Rendering retains object entry order, duplicate keys and numeric spelling. */
export function Evidence({node}:{node:JsonNode}) {
  if(node.kind==='null')return <span>null</span>;
  if(node.kind==='number')return <code>{node.lexeme}</code>;
  if(node.kind==='boolean')return <span>{String(node.value)}</span>;
  if(node.kind==='string')return <span className="lightroom-value">{node.value}</span>;
  if(node.kind==='array')return <details><summary>{node.items.length} entries</summary><ol>{node.items.map((value,index)=><li key={index}><Evidence node={value}/></li>)}</ol></details>;
  return <dl>{node.entries.map(([key,value],index)=><div key={index}><dt>{key}</dt><dd><Evidence node={value}/></dd></div>)}</dl>;
}
export type ReviewedResult={raw:string;node:JsonNode;status:Status};
export function LightroomResult({status,options,onReview}:{status:Status|null;options:Options|null;onReview:(value:ReviewedResult)=>void}) {
  const identity=status?.result_token?JSON.stringify([guardKey(status),status.attempt,status.result_token]):null;
  const current=useRef(identity);current.current=identity;
  const [page,setPage]=useState<ResultPage|null>(null),[offset,setOffset]=useState('0'),[bytes,setBytes]=useState('4096');
  const [maximum,setMaximum]=useState('1048576'),[nodes,setNodes]=useState('20000'),[depth,setDepth]=useState('64');
  const [busy,setBusy]=useState(false),[error,setError]=useState(''),[review,setReview]=useState<ReviewedResult|null>(null);
  const abort=useRef<AbortController|null>(null),epoch=useRef(0);
  const stop=()=>{epoch.current++;abort.current?.abort();abort.current=null;setBusy(false);};
  useEffect(()=>{stop();setPage(null);setReview(null);setOffset('0');setError('');return()=>{epoch.current++;abort.current?.abort();};},[identity]);
  const run=async(assemble:boolean,at:string)=>{
    if(!status?.result_token||!options||busy)return;const own=identity,generation=++epoch.current,controller=new AbortController();abort.current=controller;setBusy(true);setError('');
    try{
      const limit=decimal(bytes,'Result chunk bytes',4n,BigInt(options.chunk_bytes));const max=BigInt(decimal(maximum,'Assembly byte limit',1n,BigInt(status.limits.result_bytes)));
      const nodeLimit=Number(decimal(nodes,'AST node limit',1n,1000000n)),depthLimit=Number(decimal(depth,'AST depth limit',1n,128n));
      const assembly=new ResultAssembly({...status,token:status.result_token},max);let next=assemble?'0':decimal(at,'Result offset');
      do {const response=await lightroom({kind:'Result',guard:{workbench:status.workbench,generation:status.generation,operation:status.operation},token:status.result_token,offset:next,limit},controller.signal);if(current.current!==own||epoch.current!==generation)return;if(response.kind!=='Result')throw new Error('Unexpected result page.');const p=response.value;
        if(guardKey(p)!==guardKey(status)||p.attempt!==status.attempt||p.token!==status.result_token||p.offset!==next||p.total_bytes!==status.result_bytes)throw new Error('Result page identity or byte cursor differs.');
        setPage(p);setOffset(p.offset);if(!assemble)break;assembly.append(p);if(p.next===null){const raw=assembly.finish();const value={raw,node:parseLosslessJson(raw,{bytes:Number(max),nodes:nodeLimit,depth:depthLimit}),status};setReview(value);onReview(value);break;}next=p.next;
      }while(!controller.signal.aborted);
    }catch(e){if(current.current===own&&epoch.current===generation)setError(errorText(e));}finally{if(current.current===own&&epoch.current===generation){setBusy(false);abort.current=null;}}
  };
  return <section aria-label="Exact inspection result" className="lightroom-result"><h3>Inspection result</h3>{status?.result_token?<>
    <p>Operation {status.operation} · {status.result_bytes} exact UTF-8 bytes. Pages remain available if structured review exceeds its limits.</p>
    <div className="lightroom-fields"><label>Result byte offset<input value={offset} onChange={e=>setOffset(e.target.value)}/></label><label>Result chunk bytes<input value={bytes} onChange={e=>setBytes(e.target.value)}/></label><label>Assembly byte limit<input value={maximum} onChange={e=>setMaximum(e.target.value)}/></label><label>AST node limit<input value={nodes} onChange={e=>setNodes(e.target.value)}/></label><label>AST depth limit<input value={depth} onChange={e=>setDepth(e.target.value)}/></label></div>
    <div className="button-group"><button disabled={busy} onClick={()=>void run(false,offset)}>Read result chunk</button><button disabled={busy||!page?.next} onClick={()=>void run(false,page!.next!)}>Next result chunk</button><button disabled={busy} onClick={()=>void run(true,'0')}>Assemble bounded review</button>{busy&&<button onClick={stop}>Stop waiting for result</button>}</div>
    {page&&<><p>Bytes {page.offset}–{page.next??page.total_bytes} of {page.total_bytes}{page.next===null?' · End of this result':''}</p><textarea readOnly aria-label="Exact result fragment" value={page.json_fragment}/></>}
    {review&&<details><summary>Structured evidence — exact numeric spelling</summary><Evidence node={review.node}/></details>}
  </>:<p>No retained result for the current operation.</p>}{error&&<ErrorNotice message={error}/>}</section>;
}

import { useCallback, useEffect, useRef, useState } from 'react';
import { blake3 } from '@noble/hashes/blake3.js';
import { bytesToHex } from '@noble/hashes/utils.js';
import { errorText } from '../bridge';
import { metadataWrite, metadataWriteTerminal, type Action, type ImageIdentity, type Operation, type Review, type Status } from '../metadataWrite';

const digest=(bytes:Uint8Array)=>bytesToHex(blake3(bytes));
const attempt=()=>crypto.randomUUID();

export function useMetadataWrite(catalog:string, identity:ImageIdentity|null) {
  const [status,setStatus]=useState<Status|null>(null),[error,setError]=useState(''),[busy,setBusy]=useState(false);
  const current=useRef<Status|null>(null);
  useEffect(()=>{const abort=new AbortController();let timer:ReturnType<typeof setTimeout>;
    const poll=async()=>{try{const value=await metadataWrite(catalog,{command:'status',args:{operation:null}},'status',abort.signal);if(!abort.signal.aborted){current.current=value;setStatus(value);setError('');}}catch(e){if(!abort.signal.aborted)setError(errorText(e));}finally{if(!abort.signal.aborted)timer=setTimeout(()=>void poll(),500);}};
    void poll();return()=>{abort.abort();clearTimeout(timer);};
  },[catalog]);
  const start=useCallback(async(action:Action)=>{setBusy(true);setError('');try{const admitted=await metadataWrite(catalog,{command:'start',args:{attempt:attempt(),action}},'admitted');for(let n=0;n<120;n++){const value=await metadataWrite(catalog,{command:'status',args:{operation:admitted.operation}},'status');current.current=value;setStatus(value);if(value.operation?.id===admitted.operation&&metadataWriteTerminal(value.operation)){if(value.operation.phase!=='complete')throw new Error(value.operation.error?.message??'Metadata operation failed.');return value;}await new Promise(resolve=>setTimeout(resolve,100));}throw new Error('Metadata operation status did not settle.');}catch(e){setError(errorText(e));throw e;}finally{setBusy(false);}},[catalog]);
  const prepare=useCallback(async(edits:unknown[],base:string|null)=>{if(!identity)throw new Error('Load the selected image identity first.');const bytes=new TextEncoder().encode(JSON.stringify(edits));const hash=digest(bytes);let value=await start({kind:'input_begin',bytes:String(bytes.length),expected_blake3:hash,limits:{input_bytes:'16777216',existing_file_bytes:'16777216',evidence_bytes:'268435456',evidence_packets:'1024',alias_directories:'4096',alias_candidates:'256'}});const input=value.input!;for(let offset=0;offset<bytes.length;offset+=16384){value=await start({kind:'input_append',token:input.token,generation:input.generation,offset:String(offset),bytes:Array.from(bytes.slice(offset,offset+16384))});}await start({kind:'input_finish',token:input.token,generation:input.generation,bytes:String(bytes.length),blake3:hash});return (await start({kind:'prepare',identity,base_model:base,input:input.token,input_blake3:hash})).review!;},[identity,start]);
  const commit=useCallback(async(review:Review)=>start({kind:'commit',review:review.token,review_digest:review.digest}),[start]);
  const cancel=useCallback(async()=>{const operation:Operation|undefined=current.current?.operation??undefined;if(operation&&!metadataWriteTerminal(operation))await metadataWrite(catalog,{command:'cancel',args:{operation:operation.id,epoch:operation.epoch}},'status');},[catalog]);
  return {status,error,busy,prepare,commit,start,cancel};
}

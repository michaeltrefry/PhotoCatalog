import {useEffect,useRef,useState} from 'react';
import {CatalogError,errorText} from '../bridge';
import {inspectionTerminal,lightroom,type Guard,type InputPurpose,type InputStatus,type Options,type Request,type Status} from '../lightroom';
import {encodeInput,guardKey,inputChunk,inputObserved,utf8Length,type InputExpectation} from '../lightroomWorkflow';

type Document={raw:string;bytes:Uint8Array;purpose:InputPurpose;digest:string|null;guard:Guard};
type Pending={expected:InputExpectation;reject:(e:unknown)=>void;resolve:(value:InputStatus|null)=>void;rejected:boolean};
const inputPollCurrent=(ownerKey:string|null,latestKey:string|null,alive:boolean,read:number,epoch:number)=>alive&&ownerKey===latestKey&&read===epoch;
type InputPoll<T>={current:false}|{current:true;reply:T}|{current:true;error:unknown};
export async function observeInputPoll<T>(pending:Promise<T>,current:()=>boolean):Promise<InputPoll<T>> {
  try{const reply=await pending;return current()?{current:true,reply}:{current:false};}
  catch(error){return current()?{current:true,error}:{current:false};}
}
export const acceptInputPoll=<T>(observed:InputPoll<T>,current:()=>boolean):InputPoll<T>=>observed.current&&current()?observed:{current:false};
/** One staged slot, independently observed. Unknown acknowledgements keep the
 * slot held; a read showing old progress never authorizes replay of a write. */
export function useLightroomInput(status:Status|null,options:Options|null) {
  const guard=status&&!status.closed&&status.initialized&&inspectionTerminal(status)?{workbench:status.workbench,generation:status.generation,operation:status.operation}:null;
  const key=guard?guardKey(guard):null;
  const [value,setValue]=useState<InputStatus|null>(null),[document,setDocument]=useState<Document|null>(null),[ready,setReady]=useState(false),[busy,setBusy]=useState(false),[error,setError]=useState('');
  const context=useRef<{key:string|null;alive:boolean}>({key:null,alive:false});
  const latestKey=useRef(key);latestKey.current=key;
  const current=useRef<InputStatus|null>(null),known=useRef(false),pending=useRef<Pending|null>(null),epoch=useRef(0),doc=useRef<Document|null>(null);
  const rendered=context.current;
  useEffect(()=>{
    const own={key,alive:true};context.current=own;const abort=new AbortController();let timer:ReturnType<typeof setTimeout>;
    current.current=null;doc.current=null;known.current=false;setValue(null);setDocument(null);setReady(false);setBusy(false);setError('');
    const poll=async()=>{const read=epoch.current;try{const observed=acceptInputPoll(await observeInputPoll(lightroom({kind:'InputStatus',guard:guard!,input:null},abort.signal),()=>inputPollCurrent(own.key,latestKey.current,own.alive,read,epoch.current)),()=>inputPollCurrent(own.key,latestKey.current,own.alive,read,epoch.current));if(!observed.current)return;if('error' in observed)throw observed.error;const reply=observed.reply;if(reply.kind!=='Input')throw new Error('Unexpected staged input status.');const v=reply.value;if(v&&guardKey(v.guard)!==key)throw new Error('Stale staged input response.');current.current=v;setValue(v);known.current=true;setReady(true);setError('');const p=pending.current;if(p&&(inputObserved(p.expected,v)||p.rejected)){pending.current=null;setBusy(false);p.resolve(v);}}catch(e){if(inputPollCurrent(own.key,latestKey.current,own.alive,read,epoch.current)){known.current=false;setReady(false);setError(errorText(e));}}finally{if(own.alive&&own.key===latestKey.current)timer=setTimeout(()=>void poll(),500);}};
    if(guard)void poll();
    return()=>{own.alive=false;abort.abort();clearTimeout(timer);const p=pending.current;if(p){p.reject(new Error('Staged input scope changed; inspect the current workbench.'));pending.current=null;}};
    // Identity is the immutable three-part guard, not the changing status object.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  },[key]);
  const check=()=>{if(!rendered.alive||context.current!==rendered||rendered.key!==key||!guard||!known.current||pending.current)throw new Error('Recover staged input status before sending another command.');return guard;};
  const send=(request:Request,expected:InputExpectation):Promise<InputStatus|null>=>{
    check();const own=rendered;epoch.current++;setBusy(true);setError('');
    return new Promise((resolve,reject)=>{const ticket:Pending={expected,resolve,reject,rejected:false};pending.current=ticket;
      void lightroom(request).then(reply=>{if(!own.alive||context.current!==own||pending.current!==ticket)return;if(reply.kind!=='Input')throw new Error('Unexpected staged input acknowledgement.');if(reply.value&&guardKey(reply.value.guard)!==key)throw new Error('Stale staged input acknowledgement.');epoch.current++;if(!inputObserved(expected,reply.value))throw new Error('Staged input acknowledgement differs from the submitted bytes.');current.current=reply.value;setValue(reply.value);known.current=true;setReady(true);pending.current=null;setBusy(false);resolve(reply.value);}).catch(e=>{if(!own.alive||context.current!==own||pending.current!==ticket)return;ticket.rejected=e instanceof CatalogError;epoch.current++;known.current=false;setReady(false);setError(`${errorText(e)} Recheck the staged input; do not replay an uncertain command.`);reject(e);});
    });
  };
  return {value,document,ready,busy,error,
    retry:()=>{if(!rendered.alive||context.current!==rendered)return;epoch.current++;known.current=false;setReady(false);},
    begin:(raw:string,purpose:InputPurpose,digest:string|null)=>{const g=check();if(current.current)throw new Error('Finish using or discard the existing staged input first.');if(!options)throw new Error('Load input limits first.');if(digest&&!/^[0-9a-f]{64}$/.test(digest))throw new Error('Expected BLAKE3 must contain 64 lowercase hexadecimal digits.');if(BigInt(raw.length)>BigInt(status!.limits.request_bytes))throw new Error('Document exceeds the workbench request byte limit.');const bytes=encodeInput(raw,Number(BigInt(status!.limits.request_bytes))),total=String(bytes.length);const frozen={raw,bytes,purpose,digest,guard:g};doc.current=frozen;setDocument(frozen);return send({kind:'InputBegin',guard:g,purpose,total_bytes:total,expected_blake3:digest},{kind:'begin',purpose,total,digest});},
    append:()=>{const g=check(),v=current.current,d=doc.current;if(!v||!d||v.complete||v.purpose!==d.purpose||!options)throw new Error('No locally reviewed input is ready for another chunk.');const part=inputChunk(d.bytes,v.received_bytes,options.chunk_bytes);if(!part.fragment)throw new Error('All bytes are uploaded. Explicitly finish the input.');if(BigInt(part.next)<BigInt(v.total_bytes)&&BigInt(utf8Length(part.fragment))<BigInt(options.minimum_nonfinal_chunk_bytes))throw new Error('Nonfinal chunk is below the transport minimum.');return send({kind:'InputAppend',guard:g,input:v.input,offset:v.received_bytes,fragment:part.fragment},{kind:'append',input:v.input,next:part.next});},
    finish:()=>{const g=check(),v=current.current;if(!v||v.received_bytes!==v.total_bytes)throw new Error('Upload all declared bytes before finishing.');return send({kind:'InputFinish',guard:g,input:v.input},{kind:'finish',input:v.input});},
    discard:()=>{const g=check(),v=current.current;if(!v)throw new Error('No staged input to discard.');return send({kind:'InputDiscard',guard:g,input:v.input},{kind:'discard',input:v.input}).then(reply=>{if(rendered.alive&&context.current===rendered){doc.current=null;setDocument(null);}return reply;});},
  };
}

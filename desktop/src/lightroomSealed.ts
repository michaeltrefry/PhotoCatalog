import { displayPath, type NativePath } from './bridge';
import { sealedDocument, type SealedDocumentKind, type SealedDocumentPage, type SealedDocumentRequest } from './lightroom';
import { parseLosslessJson, stringifyLosslessJson } from './lightroomJson';
import { field, nativePath, scalar } from './lightroomWorkflow';
import { documentDigest } from './lightroomMigration';

const PAGE=16384n,MAX=16777216n;
type Send=(request:SealedDocumentRequest,signal?:AbortSignal)=>Promise<SealedDocumentPage|null>;
export type RetainedRead={session:string;document:SealedDocumentKind};
export type SealedBundle={
  directory:NativePath;seal:{text:string;blake3:string;path:NativePath};approval:{text:string;blake3:string;path:NativePath};
  policy:{text:string;blake3:string};destination:{path:NativePath;display:string};
};
const decimal=(value:string,label:string)=>{if(!/^(0|[1-9][0-9]*)$/.test(value))throw new Error(`Invalid ${label}.`);return BigInt(value);};
const same=(left:unknown,right:unknown)=>JSON.stringify(left)===JSON.stringify(right);
function checkPage(page:SealedDocumentPage,identity:{session:string;document:SealedDocumentKind;directory?:NativePath;path?:NativePath;physical?:unknown;total?:bigint;digest?:string},offset:bigint,metadata=false){
  if(page.session!==identity.session||page.document!==identity.document||!same(page.directory,identity.directory??page.directory)||!same(page.path,identity.path??page.path)||!same(page.physical,identity.physical??page.physical))throw new Error('Sealed document read identity changed.');
  const total=decimal(page.total_bytes,'sealed document length'),at=decimal(page.offset,'sealed document offset');
  if(total<1n||total>MAX||at!==offset||(identity.total!==undefined&&total!==identity.total)||(identity.digest!==undefined&&page.blake3!==identity.digest)||!/^[0-9a-f]{64}$/.test(page.blake3))throw new Error('Sealed document read bounds or digest changed.');
  if(!Array.isArray(page.bytes)||page.bytes.some(value=>!Number.isInteger(value)||value<0||value>255)||(!metadata&&page.bytes.length===0))throw new Error('Invalid sealed document byte page.');
  return total;
}

export async function discardRetainedSealedRead(session:string,send:Send=sealedDocument):Promise<void>{
  const reply=await send({action:'discard',session});
  if(reply!==null)throw new Error('Unexpected retained read discard response.');
}

export async function loadSealedDocument(directory:NativePath,document:SealedDocumentKind,signal?:AbortSignal,retained?:(value:RetainedRead|null)=>void,send:Send=sealedDocument):Promise<{text:string;blake3:string;path:NativePath;directory:NativePath}>{
  const session=crypto.randomUUID();retained?.({session,document});let primary:unknown=null;
  try{
    const first=await send({action:'begin',session,directory,document},signal);
    if(!first)throw new Error('Sealed document read was not retained.');
    const total=checkPage(first,{session,document},0n,true);
    if(first.bytes.length!==0)throw new Error('Sealed document admission returned unrequested bytes.');
    const identity={session,document,directory:first.directory,path:first.path,physical:first.physical,total,digest:first.blake3};
    const bytes=new Uint8Array(Number(total));let offset=0n;
    while(offset<total){
      const reply=await send({action:'page',session,offset:offset.toString(),limit:(total-offset<PAGE?total-offset:PAGE).toString()},signal);
      if(!reply)throw new Error('Sealed document page is absent.');
      checkPage(reply,identity,offset);
      const end=offset+BigInt(reply.bytes.length),next=reply.next===null?null:decimal(reply.next,'sealed document continuation');
      if(end>total||(end<total&&next!==end)||(end===total&&next!==null))throw new Error('Invalid sealed document continuation.');
      bytes.set(reply.bytes,Number(offset));offset=end;
    }
    const text=new TextDecoder('utf-8',{fatal:true,ignoreBOM:true}).decode(bytes);
    const encoded=new TextEncoder().encode(text);
    if(encoded.length!==bytes.length||encoded.some((value,index)=>value!==bytes[index]))throw new Error('Sealed document UTF-8 round trip changed exact bytes.');
    const digest=await documentDigest(document,text);
    if(digest!==first.blake3)throw new Error('Sealed document digest differs after exact transfer.');
    return {text,blake3:digest,path:first.path,directory:first.directory};
  }catch(error){primary=error;throw error;}
  finally{
    try{await discardRetainedSealedRead(session,send);retained?.(null);}
    catch(cleanup){if(primary===null)throw cleanup;}
  }
}

export async function loadSealedBundle(directory:NativePath,signal?:AbortSignal,retained?:(value:RetainedRead|null)=>void,send:Send=sealedDocument):Promise<SealedBundle>{
  const seal=await loadSealedDocument(directory,'seal',signal,retained,send);
  const approval=await loadSealedDocument(directory,'approval',signal,retained,send);
  if(!same(seal.directory,approval.directory))throw new Error('Seal and approval resolved to different directories.');
  const sealNode=parseLosslessJson(seal.text,{bytes:Number(MAX),nodes:200000,depth:128});
  const approvalNode=parseLosslessJson(approval.text,{bytes:Number(MAX),nodes:200000,depth:128});
  const approvalBinding=field(sealNode,'approval');if(!approvalBinding)throw new Error('The input seal has no approval binding.');
  if(scalar(field(approvalBinding,'document_blake3'))!==approval.blake3)throw new Error('The exact approval digest differs from the input seal.');
  const policyNode=field(approvalNode,'policy');if(!policyNode)throw new Error('The sealed approval has no execution policy.');
  const destinationNode=field(approvalNode,'destination');if(!destinationNode)throw new Error('The sealed approval has no destination.');
  const policyText=stringifyLosslessJson(policyNode),destination=nativePath(destinationNode);
  return {directory:seal.directory,seal,approval,policy:{text:policyText,blake3:await documentDigest('policy',policyText)},destination:{path:destination,display:displayPath(destination)}};
}

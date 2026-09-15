import type {NativePath} from './bridge';
import {artifactPreparation,type ArtifactPreparationRequest,type ArtifactPreparationReply,type ExactDocument} from './lightroom';
import {decimal} from './lightroomWorkflow';
import {documentDigest} from './lightroomMigration';
import {parseLosslessJson,stringifyLosslessJson,type JsonNode} from './lightroomJson';

const digest=/^[0-9a-f]{64}$/;
export type SelectedCapture={revision:string;manifest_blake3:string};
export type PreparedSupplement=ExactDocument;
export type RetainedArtifactPreparation={session:string;capture:SelectedCapture};

export async function prepareCaptureArtifacts(capture:SelectedCapture,directory:NativePath,maximumBytes:string,openMs:string,signal?:AbortSignal,retained?:(value:RetainedArtifactPreparation|null)=>void,send:(request:ArtifactPreparationRequest,signal?:AbortSignal)=>Promise<ArtifactPreparationReply|null>=artifactPreparation):Promise<ExactDocument[]> {
  if(!digest.test(capture.revision)||!digest.test(capture.manifest_blake3))throw new Error('Selected capture identity is invalid.');
  const session=crypto.randomUUID();retained?.({session,capture});let primary:unknown=null;
  try{
    const begun=await send({action:'begin',session,directory,capture_revision:capture.revision,manifest_blake3:capture.manifest_blake3,maximum_bytes:decimal(maximumBytes,'Maximum artifact bytes',1n),open_deadline_ms:decimal(openMs,'Artifact open milliseconds',1n,3600000n)},signal);
    if(!begun||begun.kind!=='begun'||begun.session!==session||begun.capture_revision!==capture.revision||begun.manifest_blake3!==capture.manifest_blake3)throw new Error('Artifact preparation admission identity differs.');
    const members=BigInt(decimal(begun.members,'Artifact member count',1n,16384n)),result:ExactDocument[]=[];
    for(let index=0n;index<members;index++){
      const reply=await send({action:'member',session,member_index:index.toString()},signal);
      if(!reply||reply.kind!=='prepared'||reply.session!==session||reply.member_index!==index.toString()||!digest.test(reply.input_blake3)||await documentDigest('policy',reply.input_json)!==reply.input_blake3)throw new Error('Prepared artifact identity or exact digest differs.');
      result.push({json:reply.input_json,blake3:reply.input_blake3});
    }
    return result;
  }catch(error){primary=error;throw error;}
  finally{try{const reply=await send({action:'discard',session});if(reply!==null)throw new Error('Unexpected artifact discard response.');retained?.(null);}catch(cleanup){if(primary===null)throw cleanup;}}
}
export async function discardArtifactPreparation(session:string):Promise<void>{const reply=await artifactPreparation({action:'discard',session});if(reply!==null)throw new Error('Unexpected artifact discard response.');}

export async function exactArrayDocuments(raw:string):Promise<ExactDocument[]> {
  const node=parseLosslessJson(raw,{bytes:16*1024*1024,nodes:1000000,depth:128});
  if(node.kind!=='array')throw new Error('Prepared supplements result must be an array.');
  return Promise.all(node.items.map(async item=>{const json=stringifyLosslessJson(item);return {json,blake3:await documentDigest('policy',json)};}));
}

export function approvalDraft(value:{reviewToken:string;destination:NativePath;importSource:string;overlap:'require'|'reuse';overlapReason:string;keywordOverlap:'require'|'reuse';keywordReason:string;artifacts:ExactDocument[];supplements:ExactDocument[];authorization:string}):string {
  if(!digest.test(value.reviewToken))throw new Error('Pinned selection review token is invalid.');
  if(!value.importSource.trim()||!value.authorization.trim())throw new Error('Import source label and explicit authorization are required.');
  const overlap=value.overlap==='require'?{kind:'RequireDecision'}:{kind:'ReuseExactPath',reason:value.overlapReason.trim()};
  const keyword_overlap=value.keywordOverlap==='require'?{kind:'RequireDecision'}:{kind:'ReuseExactHierarchy',reason:value.keywordReason.trim()};
  if(value.overlap==='reuse'&&!overlap.reason)throw new Error('Explain exact-path reuse.');
  if(value.keywordOverlap==='reuse'&&!keyword_overlap.reason)throw new Error('Explain exact keyword hierarchy reuse.');
  return JSON.stringify({protocol:1,review_token:value.reviewToken,destination:value.destination,import_source:value.importSource,overlap,keyword_overlap,artifacts:value.artifacts,supplements:value.supplements,authorization:value.authorization});
}

export function selectedCaptures(node:JsonNode):SelectedCapture[]{
  if(node.kind!=='object')throw new Error('Selection capture page is not an object.');
  const rows=node.entries.find(([k])=>k==='rows')?.[1];if(rows?.kind!=='array')throw new Error('Selection capture rows are absent.');
  return rows.items.filter(item=>item.kind==='object'&&item.entries.find(([k])=>k==='selected')?.[1]?.kind==='boolean'&&(item.entries.find(([k])=>k==='selected')![1] as {kind:'boolean';value:boolean}).value).map(item=>{
    const value=(key:string)=>{const n=item.kind==='object'?item.entries.find(([k])=>k===key)?.[1]:undefined;if(n?.kind!=='string')throw new Error(`Selected capture ${key} is invalid.`);return n.value;};
    return {revision:value('revision'),manifest_blake3:value('manifest_blake3')};
  });
}

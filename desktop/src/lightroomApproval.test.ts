import {describe,expect,it,vi} from 'vitest';
import {approvalDraft,exactArrayDocuments,prepareCaptureArtifacts,selectedCaptures} from './lightroomApproval';
import {parseLosslessJson} from './lightroomJson';
import {documentDigest} from './lightroomMigration';

const d=(c:string)=>c.repeat(64),path={encoding:'UnixBytes' as const,units:[47,116,109,112]};
describe('guided Lightroom approval',()=>{
  it('keeps prepared integer lexemes inside exact snippets',async()=>{
    const docs=await exactArrayDocuments('[{"pin":{"source_revision":{"length":18446744073709551615}},"evidence":"e"}]');
    expect(docs[0].json).toContain('18446744073709551615');
    expect(docs[0].blake3).toBe(await documentDigest('policy',docs[0].json));
  });
  it('builds an explicit typed approval draft',()=>{
    const raw=approvalDraft({reviewToken:d('a'),destination:path,importSource:'reviewed Lightroom',overlap:'require',overlapReason:'',keywordOverlap:'reuse',keywordReason:'same hierarchy',artifacts:[{json:'{}',blake3:d('b')}],supplements:[],authorization:'approved destination and policy'});
    expect(JSON.parse(raw)).toMatchObject({protocol:1,review_token:d('a'),overlap:{kind:'RequireDecision'},keyword_overlap:{kind:'ReuseExactHierarchy',reason:'same hierarchy'}});
  });
  it('extracts only selected capture identities',()=>{
    const node=parseLosslessJson(`{"rows":[{"revision":"${d('a')}","manifest_blake3":"${d('b')}","selected":true},{"revision":"${d('c')}","manifest_blake3":"${d('d')}","selected":false}]}`,{bytes:4096,nodes:100,depth:8});
    expect(selectedCaptures(node)).toEqual([{revision:d('a'),manifest_blake3:d('b')}]);
  });
  it('prepares every member and always discards the retained manifest',async()=>{
    const json='{"capture_revision":"x","member_index":0,"mapping":{}}',hash=await documentDigest('policy',json);let session='';
    const send=vi.fn(async(request:any)=>{if(request.action==='begin'){session=request.session;return {kind:'begun',session,directory:path,manifest_path:path,manifest_physical:{},capture_revision:d('a'),manifest_blake3:d('b'),manifest_bytes:'12',members:'1'} as const;}if(request.action==='member')return {kind:'prepared',session,member_index:'0',input_json:json,input_blake3:hash} as const;return null;});
    await expect(prepareCaptureArtifacts({revision:d('a'),manifest_blake3:d('b')},path,'100','1000',undefined,undefined,send)).resolves.toEqual([{json,blake3:hash}]);
    expect(send.mock.calls.at(-1)?.[0]).toMatchObject({action:'discard',session});
  });
});

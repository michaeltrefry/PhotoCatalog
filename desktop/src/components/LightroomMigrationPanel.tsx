import { useEffect, useRef, useState } from 'react';
import { chooseLocation, errorText, type NativePath } from '../bridge';
import { beginRequest, documentDigest, operationParts, readCompleteResult, readResultPage, terminalMigration, uploadExact, type ExactPart, type InputRole, type Operation, type Snapshot } from '../lightroomMigration';
import {exactArrayDocuments} from '../lightroomApproval';
import type {ExactDocument} from '../lightroom';
import type { useLightroomMigration } from '../state/useLightroomMigration';
import type { RetainedRead } from '../lightroomSealed';
import { Dialog, ErrorNotice, Section } from './Controls';
import './lightroom.css';

type Controller = ReturnType<typeof useLightroomMigration>;
type Location = { path: NativePath; display: string };
type Kind = Operation['operation'];
type Document = { text: string; blake3: string };
type SupplementDraft={id:string;root:Location|null;inspectionBlake3:string;captureRevision:string;sourceId:string;sourceLength:string;sourceBlake3:string;modified:string};
export type SealedSelectionLocation = { generation: number; location: Location };
const blank = (): Document => ({ text: '', blake3: '' });
const names: Record<InputRole, string> = { operation: 'Operation', seal: 'Sealed selection', approval: 'Approval', policy: 'Migration policy', repair_request: 'Repair request', supplement_requests: 'Supplement requests', execution_authorization: 'Execution authorization' };
const phaseLabel = (snapshot: Snapshot) => snapshot.phase.replaceAll('_', ' ');
const currentOnly = (kind: Kind) => ['status', 'repair_current', 'repair_status', 'repair_keywords', 'keyword_repair_status'].includes(kind);
const writesDestination = (kind: Kind) => ['run', 'prepare_supplements', 'repair_current', 'repair_keywords'].includes(kind);
const discardable = (snapshot: Snapshot) => snapshot.phase === 'uploading' || snapshot.phase === 'ready' || snapshot.phase === 'complete' || snapshot.phase === 'failed';
const READ_STORAGE='photocatalog.lightroom.sealed-read.v1';
function savedRead():RetainedRead|null{try{const value:unknown=JSON.parse(sessionStorage.getItem(READ_STORAGE)??'null');return value&&typeof value==='object'&&typeof (value as RetainedRead).session==='string'&&['seal','approval'].includes((value as RetainedRead).document)?value as RetainedRead:null;}catch{return null;}}

function operationFor(kind: Kind, values: { steps: string; seconds: string; source: string; artifact: string; artifactBytes: string; identity: string; approval: string }): Operation {
  if (kind === 'run') return { operation: kind, approval_blake3: values.approval, max_steps: values.steps, max_seconds: values.seconds, source_open_ms: values.source, artifact_open_ms: values.artifact, max_artifact_bytes: values.artifactBytes };
  if (kind === 'repair_current' || kind === 'repair_keywords') return { operation: kind, max_steps: values.steps, max_seconds: values.seconds, source_open_ms: values.source };
  if (kind === 'status') return { operation: kind, run: values.identity };
  if (kind === 'repair_status' || kind === 'keyword_repair_status') return { operation: kind, repair: values.identity };
  return { operation: 'prepare_supplements' };
}

function rolesFor(kind: Kind, authorization: boolean): InputRole[] {
  if (kind === 'run') return ['seal', 'approval', 'policy', ...(authorization ? ['execution_authorization' as const] : [])];
  if (kind === 'prepare_supplements') return ['supplement_requests'];
  if (kind === 'repair_current' || kind === 'repair_keywords') return ['seal', 'approval', 'repair_request'];
  return [];
}

export function LightroomMigrationPanel({ controller: c, catalog, catalogDisplay, sealedSelection, open, onClose, onPreparedSupplements }: { controller: Controller; catalog: string | null; catalogDisplay: string; sealedSelection: SealedSelectionLocation | null; open: boolean; onClose: () => void; onPreparedSupplements?:(value:ExactDocument[])=>void }) {
  const [kind, setKind] = useState<Kind>('run');
  const [destinationMode, setDestinationMode] = useState<'new' | 'current'>('new');
  const [destination, setDestination] = useState<Location | null>(null);
  const [documents, setDocuments] = useState<Record<InputRole, Document>>({ operation: blank(), seal: blank(), approval: blank(), policy: blank(), repair_request: blank(), supplement_requests: blank(), execution_authorization: blank() });
  const [includeAuthorization, setIncludeAuthorization] = useState(false);
  const [steps, setSteps] = useState('1000000'), [seconds, setSeconds] = useState('3600'), [sourceOpen, setSourceOpen] = useState('30000'), [artifactOpen, setArtifactOpen] = useState('30000'), [artifactBytes, setArtifactBytes] = useState('1073741824'), [timeout, setTimeoutValue] = useState('3900000');
  const [identity, setIdentity] = useState(''), [reviewed, setReviewed] = useState(false), [authorizationReviewed, setAuthorizationReviewed] = useState(false);
  const [page, setPage] = useState('0'), [result, setResult] = useState(''), [localError, setLocalError] = useState(''), [notice, setNotice] = useState('');
  const [supplementDrafts,setSupplementDrafts]=useState<SupplementDraft[]>([]);
  const [sealedLocation, setSealedLocation] = useState<Location | null>(null), [sealedBusy, setSealedBusy] = useState(false), [retainedRead, setRetainedRead] = useState<RetainedRead | null>(savedRead);
  const frozen = useRef<ExactPart[] | null>(null);
  const admission = useRef<ReturnType<typeof beginRequest> | null>(null);
  const sealedAbort = useRef<AbortController | null>(null), sealedReading = useRef(false), loadedSeed = useRef(0);
  const value = c.snapshot;
  const existing = currentOnly(kind) || ((kind === 'run' || kind === 'prepare_supplements') && destinationMode === 'current');
  const roles = rolesFor(kind, includeAuthorization);
  const run = async (action: () => Promise<void>) => { setLocalError(''); try { await action(); } catch (error) { setLocalError(errorText(error)); } };
  const rememberRead = (value: RetainedRead | null) => { setRetainedRead(value); try { if(value)sessionStorage.setItem(READ_STORAGE,JSON.stringify(value));else sessionStorage.removeItem(READ_STORAGE); } catch { /* Backend custody remains visible in component state. */ } };
  const loadSelection = async (location: Location) => {
    if (sealedReading.current || retainedRead) { setLocalError('Finish or discard the retained sealed-document read first.'); return; }
    const abort = new AbortController(); sealedAbort.current = abort; sealedReading.current = true; setSealedBusy(true); setLocalError(''); setNotice('');
    try {
      const { loadSealedBundle } = await import('../lightroomSealed');
      const bundle = await loadSealedBundle(location.path, abort.signal, rememberRead);
      setDocuments(current => ({ ...current, seal: bundle.seal, approval: bundle.approval, policy: bundle.policy }));
      setDestination(bundle.destination); setSealedLocation({ path: bundle.directory, display: location.display }); setKind('run'); setIncludeAuthorization(false);
      setDestinationMode(catalog ? 'current' : 'new'); setReviewed(false); setAuthorizationReviewed(false); frozen.current = null;
      setNotice('Loaded exact input-seal.json and approval.json through the isolated filesystem owner. The policy and approved destination below come from that exact approval; review them before admission.');
    } catch (error) { if (!(error instanceof DOMException && error.name === 'AbortError')) setLocalError(errorText(error)); }
    finally { if (sealedAbort.current === abort) sealedAbort.current = null; sealedReading.current = false; setSealedBusy(false); }
  };
  useEffect(() => {
    if (open && sealedSelection && sealedSelection.generation !== loadedSeed.current && !c.snapshot) {
      loadedSeed.current = sealedSelection.generation; setSealedLocation(sealedSelection.location); void loadSelection(sealedSelection.location);
    }
  }, [open, sealedSelection?.generation]);
  const close = () => { sealedAbort.current?.abort(); onClose(); };
  const chooseDestination = () => run(async () => {
    const choice = await chooseLocation(existing ? 'lightroom_approval_destination' : 'lightroom_new_approval_destination');
    if (choice) { setDestination(choice); setReviewed(false); setAuthorizationReviewed(false); }
  });
  const start = () => run(async () => {
    if (!destination) throw new Error('Choose and review the exact destination first.');
    if (existing && !catalog) throw new Error('Open the exact destination catalog before this operation.');
    if (!reviewed) throw new Error('Acknowledge the exact destination and immutable source documents.');
    if (writesDestination(kind) && !authorizationReviewed) throw new Error('Acknowledge the destination write and policy boundary.');
    const operation = operationFor(kind, { steps, seconds, source: sourceOpen, artifact: artifactOpen, artifactBytes, identity, approval: documents.approval.blake3 });
    const input = Object.fromEntries(roles.map(role => [role, documents[role]])) as Partial<Record<InputRole, Document>>;
    const parts = operationParts(operation, input); frozen.current = parts;
    const request = beginRequest(destination.path, existing ? catalog : null, operation, parts, timeout); admission.current = request;
    const reply = await c.request(request);
    if (reply.kind !== 'status') throw new Error('Unexpected migration admission response.');
    setNotice(parts.length ? 'Exact document roster admitted. Upload each frozen byte before execution.' : 'Operation admitted without documents. Execute it explicitly when ready.');
  });
  const upload = () => run(async () => {
    if (!c.current.current || !frozen.current) throw new Error('This UI does not retain the exact documents for the guarded upload. Cancel or discard it rather than reconstructing authority.');
    await uploadExact(c.current.current, frozen.current, c.request); setNotice('All exact documents were verified by BLAKE3. Review, then execute explicitly.');
  });
  const act = () => run(async () => { if (!c.current.current) throw new Error('No guarded migration is owned.'); const reply = await c.request({ action: 'act', guard: c.current.current.guard }); if (reply.kind !== 'status') throw new Error('Unexpected execute response.'); setNotice('Execution requested. Status remains visible if this panel closes.'); });
  const guarded = (action: 'status' | 'cancel' | 'retry_drain' | 'discard') => run(async () => {
    if (!c.current.current) throw new Error('No guarded migration is owned.');
    const reply = await c.request({ action, guard: c.current.current.guard });
    if (action === 'discard' && reply.kind === 'discarded') { frozen.current = null; admission.current = null; setResult(''); setNotice('Retained migration inputs and result were discarded.'); }
  });
  const readPage = () => run(async () => { if (!c.current.current) throw new Error('No complete result is owned.'); setResult(await readResultPage(c.current.current, page, c.request)); });
  const retainSupplements=()=>run(async()=>{if(kind!=='prepare_supplements'||!c.current.current)throw new Error('No complete supplement result is owned.');const raw=await readCompleteResult(c.current.current,c.request);const exact=await exactArrayDocuments(raw);onPreparedSupplements?.(exact);setResult(raw);setNotice(`Retained ${exact.length} exact prepared supplement proof documents for the guided approval factory.`);});
  const stageSupplementRequests=()=>run(async()=>{if(!supplementDrafts.length)throw new Error('Add at least one supplement proof.');const path=(root:Location)=>root.path.encoding==='UnixBytes'?{encoding:'UnixBytes' as const,units:Array.from(new TextEncoder().encode('inspection.json'))}:{encoding:'WindowsWide' as const,units:Array.from('inspection.json',c=>c.charCodeAt(0))};const number=(value:string,label:string,optional=false)=>{if(optional&&value==='')return 'null';if(!/^(0|[1-9][0-9]*)$/.test(value))throw new Error(`${label} must be a canonical whole decimal.`);return value;};const exact=(value:string,label:string)=>{if(!/^[0-9a-f]{64}$/.test(value))throw new Error(`${label} must be a lowercase BLAKE3 digest.`);return value;};const snippets=supplementDrafts.map((draft,index)=>{if(!draft.root)throw new Error(`Choose supplement proof folder ${index+1}.`);return `{"proof_root":${JSON.stringify(draft.root.path)},"inspection_relative":${JSON.stringify(path(draft.root))},"inspection_blake3":${JSON.stringify(exact(draft.inspectionBlake3,'Inspection'))},"capture_revision":${JSON.stringify(exact(draft.captureRevision,'Capture revision'))},"source_id":${JSON.stringify(draft.sourceId)},"source_revision":{"length":${number(draft.sourceLength,'Source length')},"blake3":${JSON.stringify(exact(draft.sourceBlake3,'Source revision'))},"modified_unix_ns":${number(draft.modified,'Modified nanoseconds',true)}},"historical_status":"Malformed"}`;});const text=`[${snippets.join(',')}]`,blake3=await documentDigest('supplement_requests',text);setDocuments(current=>({...current,supplement_requests:{text,blake3}}));setReviewed(false);setAuthorizationReviewed(false);setNotice('Built exact supplement requests from the reviewed controls. Review the generated document before admission.');});
  if (!open) return null;
  return <Dialog title="Lightroom migration and recovery" onClose={close}><div className="lightroom-panel">
    <p>Run a sealed Lightroom migration, prepare supplement evidence, or inspect and repair a migration already written to the open catalog. Originals, Lightroom catalogs, and source XMP remain unchanged. Every destination write requires an explicit reviewed operation.</p>
    {(localError || c.error) && <ErrorNotice message={localError || c.error} dismiss={() => setLocalError('')} />}
    {c.statusError && <ErrorNotice message={`Guarded status check: ${c.statusError}`} />}
    {notice && <p role="status">{notice}</p>}
    {c.outcomeUnknown && <p role="alert">The last transport acknowledgement is unknown. Do not replay a different operation. Guarded polling will reconcile the retained operation when possible.{!value && admission.current ? ' The identical admission can be retried safely with its retained operation ID and header.' : ''}</p>}
    {c.outcomeUnknown && !value && admission.current && <button disabled={c.busy} onClick={() => void run(async () => { const reply = await c.request(admission.current!); if (reply.kind !== 'status') throw new Error('Unexpected migration admission response.'); })}>Retry identical guarded admission</button>}
    {c.stale && <section aria-label="Stale migration ownership"><p role="alert">The saved guard no longer identifies the backend slot. The backend may have restarted or another owner may exist. Forgetting this reference does not cancel or discard backend work.</p><button onClick={c.forgetStale}>Forget stale local reference</button></section>}
    <Section title="Load a sealed selection">
      <p>Choose an existing seal folder to load its fixed <code>input-seal.json</code> and <code>approval.json</code>. The isolated filesystem owner reads and hashes each file once, retains a bounded byte snapshot while paging, then discards it explicitly. The derived policy keeps exact numeric lexemes and key order; the original seal and approval bytes are never reserialized.</p>
      <button disabled={sealedBusy || !!value || c.busy || !!retainedRead} onClick={() => void run(async () => { const location = await chooseLocation('lightroom_seal'); if (location) { setSealedLocation(location); await loadSelection(location); } })}>Choose and load sealed selection…</button>
      {sealedLocation && <p className="source-path">{sealedLocation.display}</p>}
      {sealedBusy && <p role="status">Reading and hashing a bounded sealed document… <button onClick={() => sealedAbort.current?.abort()}>Cancel read</button></p>}
      {retainedRead && <p role="alert">Filesystem read session <code>{retainedRead.session}</code> for {retainedRead.document} may still retain its bounded snapshot. <button disabled={sealedBusy} onClick={() => void run(async () => { const { discardRetainedSealedRead } = await import('../lightroomSealed'); await discardRetainedSealedRead(retainedRead.session); rememberRead(null); setNotice('Retained sealed-document snapshot discarded.'); })}>Retry exact discard</button> <button disabled={sealedBusy} onClick={() => { rememberRead(null); setNotice('Forgot the local read reference. This does not discard a snapshot still retained by the backend.'); }}>Forget stale local reference</button></p>}
    </Section>
    <Section title="Operation and destination">
      <label>Migration operation<select value={kind} disabled={!!value || c.busy || c.outcomeUnknown} onChange={event => { const next = event.target.value as Kind; setKind(next); setDestination(null); setDestinationMode(currentOnly(next) ? 'current' : 'new'); setReviewed(false); setAuthorizationReviewed(false); setResult(''); }}>
        <option value="run">Run approved migration</option><option value="status">Read migration run status</option><option value="prepare_supplements">Prepare supplement requests</option><option value="repair_current">Repair current-photo state</option><option value="repair_status">Read current-state repair status</option><option value="repair_keywords">Repair keyword state</option><option value="keyword_repair_status">Read keyword repair status</option>
      </select></label>
      {(kind === 'run' || kind === 'prepare_supplements') && <label>Destination ownership<select value={destinationMode} disabled={!!value || c.busy} onChange={event => { setDestinationMode(event.target.value as typeof destinationMode); setDestination(null); setReviewed(false); }}><option value="new">New LensWorks catalog</option><option value="current">Currently open LensWorks catalog</option></select></label>}
      {currentOnly(kind) && <p>The operation is bound to the currently open catalog token. Choose that same catalog folder below so the backend can verify its native identity.</p>}
      {existing && !catalog && <p role="alert">Open the destination LensWorks catalog before using this operation.</p>}
      {!existing && catalog && <p role="alert">Close the currently open catalog before admitting a different new migration destination.</p>}
      <button disabled={!!value || c.busy || (existing && !catalog)} onClick={() => void chooseDestination()}>{existing ? 'Choose open destination folder…' : 'Choose new destination…'}</button>
      {destination && <p className="source-path">{destination.display}</p>}
      {existing && catalog && <p className="hint">Open catalog: {catalogDisplay || catalog} · opaque token <code>{catalog}</code></p>}
      {(kind === 'status' || kind === 'repair_status' || kind === 'keyword_repair_status') && <label>{kind === 'status' ? 'Exact run ID' : 'Exact repair ID'}<input value={identity} disabled={!!value || c.busy} onChange={event => setIdentity(event.target.value)} /></label>}
    </Section>
    {(kind === 'run' || kind === 'repair_current' || kind === 'repair_keywords') && <Section title="Execution limits" open={false}><fieldset disabled={!!value || c.busy} className="lightroom-fields"><label>Maximum steps<input inputMode="numeric" value={steps} onChange={event => setSteps(event.target.value)} /></label><label>Maximum seconds<input inputMode="numeric" value={seconds} onChange={event => setSeconds(event.target.value)} /></label><label>Source open milliseconds<input inputMode="numeric" value={sourceOpen} onChange={event => setSourceOpen(event.target.value)} /></label>{kind === 'run' && <><label>Artifact open milliseconds<input inputMode="numeric" value={artifactOpen} onChange={event => setArtifactOpen(event.target.value)} /></label><label>Maximum artifact bytes<input inputMode="numeric" value={artifactBytes} onChange={event => setArtifactBytes(event.target.value)} /></label></>}</fieldset></Section>}
    <Section title="Exact immutable documents">
      {kind==='prepare_supplements'&&<details open><summary>Guided qualified PSD supplement proofs</summary><p>Add each explicitly chosen proof folder containing its fixed <code>inspection.json</code>. Enter identities from the pinned Selection sources and original evidence views; the guarded backend rejects any proof that differs.</p>{supplementDrafts.map((draft,index)=><fieldset key={draft.id}><legend>Supplement proof {index+1}</legend><button onClick={()=>void run(async()=>{const location=await chooseLocation('lightroom_capture_evidence');if(location)setSupplementDrafts(rows=>rows.map(row=>row.id===draft.id?{...row,root:location}:row));})}>Choose proof folder…</button><span>{draft.root?.display??'No folder selected'}</span><div className="lightroom-fields">{([['Inspection BLAKE3','inspectionBlake3'],['Selected capture revision','captureRevision'],['Selected source ID','sourceId'],['Source byte length','sourceLength'],['Source BLAKE3','sourceBlake3'],['Source modified nanoseconds (blank if absent)','modified']] as const).map(([label,key])=><label key={key}>{label}<input value={draft[key]} onChange={e=>setSupplementDrafts(rows=>rows.map(row=>row.id===draft.id?{...row,[key]:e.target.value}:row))}/></label>)}</div><button onClick={()=>setSupplementDrafts(rows=>rows.filter(row=>row.id!==draft.id))}>Remove proof</button></fieldset>)}<button onClick={()=>setSupplementDrafts(rows=>[...rows,{id:crypto.randomUUID(),root:null,inspectionBlake3:'',captureRevision:'',sourceId:'',sourceLength:'',sourceBlake3:'',modified:''}])}>Add supplement proof</button><button disabled={!supplementDrafts.length} onClick={()=>void stageSupplementRequests()}>Build exact supplement request document</button></details>}
      {kind === 'run' && <label className="checkbox"><input type="checkbox" checked={includeAuthorization} disabled={!!value || c.busy} onChange={event => { setIncludeAuthorization(event.target.checked); setReviewed(false); setAuthorizationReviewed(false); }} />Include a separate execution-authorization document after the policy</label>}
      {roles.length === 0 ? <p>This read-only status operation has no uploaded document roster.</p> : roles.map(role => <div key={role} className="migration-document"><label>{names[role]} JSON<textarea value={documents[role].text} disabled={!!value || c.busy} onChange={event => { setDocuments(current => ({ ...current, [role]: { text: event.target.value, blake3: '' } })); setReviewed(false); setAuthorizationReviewed(false); }} /></label><label>{names[role]} BLAKE3<input value={documents[role].blake3} readOnly spellCheck={false} /></label><button disabled={!!value || c.busy || !documents[role].text} onClick={() => void run(async () => { const text = documents[role].text, digest = await documentDigest(role, documents[role].text); setDocuments(current => current[role].text === text ? { ...current, [role]: { text, blake3: digest } } : current); setReviewed(false); setAuthorizationReviewed(false); })}>Compute exact document BLAKE3</button></div>)}
      <p className="hint">Documents are uploaded in the backend-required order as exact UTF-8. Digests are verified by the backend before execution; LensWorks does not parse and reserialize authority documents in the UI.</p>
    </Section>
    <Section title="Review and authorize">
      <label>Overall operation timeout milliseconds<input inputMode="numeric" value={timeout} disabled={!!value || c.busy} onChange={event => { setTimeoutValue(event.target.value); setReviewed(false); }} /></label>
      <label className="checkbox"><input type="checkbox" checked={reviewed} disabled={!!value || c.busy || !destination} onChange={event => setReviewed(event.target.checked)} />I reviewed the exact destination, operation limits, immutable document bytes, and BLAKE3 values shown above.</label>
      {writesDestination(kind) && <label className="checkbox"><input type="checkbox" checked={authorizationReviewed} disabled={!!value || c.busy || !reviewed} onChange={event => setAuthorizationReviewed(event.target.checked)} />I authorize this bounded operation to write only the selected LensWorks destination under the reviewed policy. This does not authorize changes to originals, Lightroom catalogs, or source XMP.</label>}
      <button className="primary" disabled={!!value || c.busy || c.stale || c.outcomeUnknown || !destination || (!existing && !!catalog) || !reviewed || (writesDestination(kind) && !authorizationReviewed)} onClick={() => void start()}>Begin guarded operation</button>
    </Section>
    <Section title="Guarded status and controls">
      {!c.ready && <p role="status">Checking retained migration ownership…</p>}
      {!value ? <p>No migration guard is owned by this UI. Another backend owner cannot be discovered without its exact guard; a conflicting begin is refused as busy.</p> : <>
        <p role="status">{phaseLabel(value)} · {value.uploaded} uploaded bytes</p><p className="hint">Session <code>{value.guard.session}</code> · generation <code>{value.guard.generation}</code> · operation <code>{value.guard.operation}</code></p>
        {value.progress && <p>{value.progress[0]} · {value.progress[1]}{value.progress[2] ? ` of ${value.progress[2]}` : ''}</p>}
        {value.failure && <ErrorNotice message={`${value.failure.code}: ${value.failure.detail}${value.failure.outcome_unknown ? ' Outcome may be unknown; retain custody and retry drain.' : ''}${value.failure.poisoned ? ' Worker state is poisoned.' : ''}`} />}
        {value.result && <p>Checked result: {value.result.bytes} bytes · {value.result.pages} pages · BLAKE3 <code>{value.result.blake3}</code></p>}
        <div className="button-group"><button disabled={c.busy} onClick={() => void guarded('status')}>Recheck status</button><button disabled={c.busy || !frozen.current || value.phase !== 'uploading'} onClick={() => void upload()}>Upload remaining exact documents</button><button className="primary" disabled={c.busy || value.phase !== 'ready'} onClick={() => void act()}>Execute reviewed operation</button></div>
        <div className="button-group"><button disabled={c.busy || terminalMigration(value)} onClick={() => void guarded('cancel')}>Cancel operation</button><button disabled={c.busy || !(value.phase === 'drain_pending' || value.failure?.outcome_unknown)} onClick={() => void guarded('retry_drain')}>Retry checked drain</button><button disabled={c.busy || !discardable(value)} onClick={() => void guarded('discard')}>Discard retained operation and result</button></div>
      </>}
    </Section>
    {value?.result && <Section title="Paged exact result"><label>Result page<input inputMode="numeric" value={page} onChange={event => setPage(event.target.value)} /></label><button disabled={c.busy} onClick={() => void readPage()}>Load complete page</button>{kind==='prepare_supplements'&&<button disabled={c.busy} onClick={()=>void retainSupplements()}>Use exact prepared proofs in approval factory</button>}{result && <textarea readOnly aria-label="Exact migration result page" value={result} />}</Section>}
    <p className="hint">Closing this panel keeps the guarded operation and status polling active. Closing its destination catalog requests cancellation and waits for checked drain; quitting LensWorks cancels and joins native migration work.</p>
  </div></Dialog>;
}

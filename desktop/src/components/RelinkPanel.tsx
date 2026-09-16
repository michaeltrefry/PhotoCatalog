import { useEffect, useRef, useState, type ReactNode } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { errorText, type GridImage, type NativePath } from '../bridge';
import { enteredReference, identityLabel, pathLabel, referenceLabel, relink, relinkTerminal, type Mounts, type Original, type PathReference, type RelinkData, type RelinkOverride, type RelinkPlan, type RelinkRequest, type RuleCursor } from '../relink';
import type { useRelink } from '../state/useRelink';
import { Dialog, ErrorNotice } from './Controls';
import './metadata.css';

type Controller = ReturnType<typeof useRelink>;
type Mutation = (action: () => Promise<void>) => Promise<void>;
type Picker = { path: NativePath; display: string };
async function choosePath(folder: boolean): Promise<Picker | null> {
  return invoke('catalog_choose_location', { purpose: folder ? 'relink_folder' : 'relink_original' });
}

function LazyDetails({ title, children }: { title: string; children: ReactNode }) {
  const [open, setOpen] = useState(false);
  return <details onToggle={e => setOpen(e.currentTarget.open)}><summary>{title}</summary>{open && children}</details>;
}

function Destinations({ folder, value, onChange, disabled }: { folder: boolean; value: NativePath[]; onChange: (paths: NativePath[]) => void; disabled: boolean }) {
  const [error, setError] = useState(''); const [picking, setPicking] = useState(false); const mounted = useRef(true);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  return <div>{value.map((path, index) => <p key={index}>{pathLabel(path)} <button disabled={disabled || picking} onClick={() => onChange(value.filter((_, i) => i !== index))}>Remove destination {index + 1}</button></p>)}
    <button disabled={disabled || picking || value.length >= 32} onClick={() => { setPicking(true); setError(''); void choosePath(folder).then(choice => { if (mounted.current && choice) onChange([...value, choice.path]); }).catch(e => { if (mounted.current) setError(errorText(e)); }).finally(() => { if (mounted.current) setPicking(false); }); }}>{picking ? 'Choosing…' : folder ? 'Choose destination folder…' : 'Choose original file…'}</button>{error && <ErrorNotice message={error} />}</div>;
}

function EnteredFolder({ onChange }: { onChange: (path: PathReference | null) => void }) {
  const [value, setValue] = useState(''); const [platform, setPlatform] = useState<'unix' | 'windows'>('unix'); const [error, setError] = useState('');
  const update = (next: string, format: 'unix' | 'windows') => { setValue(next); setPlatform(format); try { onChange(enteredReference(next, format)); setError(''); } catch (e) { onChange(null); setError(next ? errorText(e) : ''); } };
  return <><label className="form-field">Existing folder path<input value={value} maxLength={8192} onChange={e => update(e.target.value, platform)} placeholder="/Volumes/Previous Drive/Raw" /></label><label className="form-field">Path format<select value={platform} onChange={e => update(value, e.target.value as 'unix' | 'windows')}><option value="unix">macOS / Linux</option><option value="windows">Windows</option></select></label>{error && <ErrorNotice message={error} />}</>;
}

type PageKind = 'plans' | 'items' | 'sources' | 'rules';
type Cursor = string | RuleCursor | null;
function ReviewPages<K extends PageKind>({ catalog, kind, request, first, children }: { catalog: string; kind: K; request: (cursor: Cursor) => RelinkRequest; first: Cursor; children: (row: RelinkData[K]['rows'][number]) => ReactNode }) {
  const [cursor, setCursor] = useState<Cursor>(first); const [epoch, setEpoch] = useState(0); const [page, setPage] = useState<RelinkData[K] | null>(null); const [busy, setBusy] = useState(false); const [error, setError] = useState('');
  const query = JSON.stringify(request(cursor));
  useEffect(() => { const abort = new AbortController(); setBusy(true); setPage(null); setError(''); void relink(catalog, JSON.parse(query), kind, abort.signal).then(value => { if (!abort.signal.aborted) setPage(value); }).catch(e => { if (!abort.signal.aborted) setError(errorText(e)); }).finally(() => { if (!abort.signal.aborted) setBusy(false); }); return () => abort.abort(); }, [catalog, query, kind, epoch]);
  return <section>{busy && <p role="status">Loading saved review…</p>}{error && <ErrorNotice message={error} />}{page && <>{page.rows.map((row, index) => <article key={index}>{children(row)}</article>)}{!page.rows.length && <p>{page.next === null ? 'No entries on this page.' : 'No matching entries in this batch. Continue reviewing the next batch.'}</p>}</>}
    <div className="button-group"><button disabled={busy} onClick={() => { setCursor(first); setEpoch(v => v + 1); }}>First {kind} page / Refresh</button><button disabled={busy || !page || page.next === null} onClick={() => setCursor(page!.next)}>Next {kind} page</button></div></section>;
}

function RuleDescription({ row }: { row: RelinkData['rules']['rows'][number] }) {
  const rule = row.rule;
  if (rule.kind === 'scope') {
    const scope = rule.value;
    if (scope.scope === 'prefix') return <><p>Folder: {referenceLabel(scope.value.from)}</p>{scope.value.destinations.map((path, i) => <p key={i}>Destination: {pathLabel(path)}</p>)}</>;
    if (scope.scope === 'volume') return <><p>Reconnect volume to {pathLabel(scope.value.mount_path)} · {scope.value.filesystem}</p><details><summary>Logical volume identity</summary>{scope.value.logical_volume}</details></>;
    return <><p>Original: {row.label ?? 'Saved original association'}</p>{scope.value.destinations.map((path, i) => <p key={i}>Destination: {pathLabel(path)}</p>)}<details><summary>Original identity</summary>{scope.value.asset_id}</details></>;
  }
  const override = rule.value;
  const paths = override.target === 'prefix' ? override.value.destinations : override.value.candidates;
  return <><p>{override.target === 'prefix' ? `Nested folder: ${referenceLabel(override.value.from)}` : `${override.target === 'asset' ? 'Original' : 'Metadata source'}: ${row.label ?? 'Retained association'}`}</p>{paths.length ? paths.map((path, i) => <p key={i}>Destination: {pathLabel(path)}</p>) : <p>Excluded from this review</p>}{override.target !== 'prefix' && <details><summary>Association identity</summary>{override.target === 'asset' ? override.value.asset_id : override.value.source_id}</details>}</>;
}

function CandidateChange({ folder, busy, apply }: { folder: boolean; busy: boolean; apply: (paths: NativePath[]) => Promise<void> }) {
  const [paths, setPaths] = useState<NativePath[]>([]); const [error, setError] = useState('');
  return <details><summary>Change this association</summary><Destinations folder={folder} value={paths} onChange={setPaths} disabled={busy} /><div className="button-group"><button disabled={busy || !paths.length} onClick={() => { setError(''); void apply(paths).catch(e => setError(errorText(e))); }}>Create revised review with these candidates</button><button disabled={busy} onClick={() => { setError(''); void apply([]).catch(e => setError(errorText(e))); }}>Exclude from revised review</button></div>{error && <ErrorNotice message={error} />}</details>;
}

function PlanReview({ catalog, plan, busy, run, revise }: { catalog: string; plan: RelinkPlan; busy: boolean; run: (request: RelinkRequest) => Promise<void>; revise: (changes: RelinkOverride[]) => Promise<void> }) {
  const [acknowledged, setAcknowledged] = useState(false); const [error, setError] = useState(''); const [from, setFrom] = useState<PathReference | null>(null); const [paths, setPaths] = useState<NativePath[]>([]);
  const submit = (request: RelinkRequest) => { setError(''); void run(request).catch(e => setError(errorText(e))); };
  return <section><h3>Saved relink review</h3><p>State: {plan.state} · revision {plan.revision}</p><details><summary>Review identifier</summary>{plan.id}</details>
    {plan.summary_complete ? <p>{plan.total} originals · {plan.matched} matched · {plan.unverified} need association confirmation · {plan.user_confirmed} confirmed · {plan.excluded} excluded · {plan.unresolved} unresolved originals · {plan.unresolved_sources} unresolved metadata sources.</p> : <p>Aggregate counts are unavailable for this older review. Create a fresh review to prepare or apply it. An applied review can still be undone after lineage validation.</p>}
    {error && <ErrorNotice message={error} />}
    <LazyDetails title="Review folder mappings and exceptions"><ReviewPages catalog={catalog} kind="rules" first={null} request={after => ({ command: 'rules', args: { plan: plan.id, revision: plan.revision, after: after as RuleCursor | null, limit: '20' } })}>{row => <RuleDescription row={row} />}</ReviewPages></LazyDetails>
    <div className="button-group"><button disabled={busy || !['preparing', 'checking'].includes(plan.state)} onClick={() => submit({ command: 'prepare', args: { plan: plan.id, revision: plan.revision, batch_rows: '64' } })}>Prepare saved review</button><button disabled={busy} onClick={() => { setError(''); void revise([]).catch(e => setError(errorText(e))); }}>Create a fresh review with these mappings</button></div>
    {plan.summary_complete && plan.state === 'ready' && plan.confirmation_token && <section><p>This confirmation covers all {plan.unverified} unverified original associations in this saved review, including rows outside the visible page. Check its folder mappings, exceptions, and candidate files first.</p><label className="checkbox"><input type="checkbox" checked={acknowledged} disabled={busy} onChange={e => setAcknowledged(e.target.checked)} />I reviewed these associations. No retained original digest proves they are the historic originals.</label><button disabled={busy || !acknowledged} onClick={() => submit({ command: 'confirm', args: { plan: plan.id, revision: plan.revision, token: plan.confirmation_token!, acknowledgement: 'no_retained_original_digest' } })}>Confirm reviewed associations</button></section>}
    <div className="button-group"><button disabled={busy || !plan.summary_complete || plan.state !== 'ready' || plan.unresolved !== '0' || plan.unresolved_sources !== '0'} onClick={() => submit({ command: 'apply', args: { plan: plan.id, revision: plan.revision } })}>Apply reviewed catalog paths</button><button disabled={busy || plan.state !== 'applied'} onClick={() => submit({ command: 'undo', args: { plan: plan.id, revision: plan.revision } })}>Undo this relink</button></div><p className="hint">Apply changes catalog associations. Originals stay on disk. Undo validates that subsequent catalog changes still permit reversal.</p>
    <details><summary>Add a nested folder mapping</summary><EnteredFolder onChange={setFrom} /><Destinations folder value={paths} onChange={setPaths} disabled={busy} /><button disabled={busy || !from || !paths.length} onClick={() => { setError(''); void revise([{ target: 'prefix', value: { from: from!, destinations: paths } }]).catch(e => setError(errorText(e))); }}>Create revised folder review</button></details>
    <h3>Originals in this review</h3><ReviewPages catalog={catalog} kind="items" first="0" request={after => ({ command: 'items', args: { plan: plan.id, revision: plan.revision, after: after as string, limit: '20' } })}>{row => <>
      <p>{referenceLabel(row.original)}</p><p>{row.status}: {row.detail}</p><p>{identityLabel(row.identity_basis)}</p>{row.destination && <p>Reviewed destination: {pathLabel(row.destination)}</p>}{row.candidates.map((candidate, i) => <p key={i}>{pathLabel(candidate.path)} · {candidate.status}: {candidate.detail}</p>)}
      <CandidateChange folder={false} busy={busy} apply={candidates => revise([{ target: 'asset', value: { asset_id: row.asset_id, candidates } }])} />
      <LazyDetails title="Related metadata source associations"><ReviewPages catalog={catalog} kind="sources" first="0" request={after => ({ command: 'sources', args: { plan: plan.id, revision: plan.revision, sequence: row.sequence, after: after as string, limit: '20' } })}>{source => <><p>{referenceLabel(source.original)}</p><p>{source.status}: {source.detail}</p>{source.destination && <p>Destination: {pathLabel(source.destination)}</p>}{source.candidates.map((candidate, i) => <p key={i}>{pathLabel(candidate.path)} · {candidate.status}: {candidate.detail}</p>)}<CandidateChange folder={false} busy={busy} apply={candidates => revise([{ target: 'source', value: { source_id: source.source_id, candidates } }])} /></>}</ReviewPages></LazyDetails>
    </>}</ReviewPages>
  </section>;
}

export function RelinkPanel({ catalog, selected, open, onClose, controller, mutate, changed }: { catalog: string; selected: GridImage | null; open: boolean; onClose: () => void; controller: Controller; mutate: Mutation; changed: () => void }) {
  const [plan, setPlan] = useState<RelinkPlan | null>(null); const [epoch, setEpoch] = useState(0); const [error, setError] = useState(''); const [from, setFrom] = useState<PathReference | null>(null); const [destinations, setDestinations] = useState<NativePath[]>([]); const [files, setFiles] = useState<NativePath[]>([]); const [original, setOriginal] = useState<Original | null>(null); const [mounts, setMounts] = useState<Mounts | null>(null);
  const [admitting, setAdmitting] = useState(false); const admission = useRef(false);
  const selectedKey = selected ? JSON.stringify(selected.key) : '';
  useEffect(() => { setFiles([]); setOriginal(null); }, [selectedKey]);
  const seen = useRef<string | null>(null); const changedRef = useRef(changed); changedRef.current = changed;
  const operation = controller.operation;
  useEffect(() => {
    if (!operation || !relinkTerminal(operation) || seen.current === operation.id) return;
    seen.current = operation.id;
    if (operation.plan) setPlan(operation.plan);
    if (operation.result?.kind === 'plan') setPlan(operation.result.value);
    if (operation.result?.kind === 'mounts') setMounts(operation.result.value);
    if (operation.result?.kind === 'original') setOriginal(operation.result.value);
    setEpoch(v => v + 1);
    if (operation.phase === 'complete' && ['apply', 'undo'].includes(operation.action)) changedRef.current();
  }, [operation]);
  const busy = admitting || controller.busy || !controller.ready;
  const run = (request: RelinkRequest) => mutate(() => controller.start(request));
  const revise = async (changes: RelinkOverride[]) => { if (!plan) throw new Error('Open a saved review first.'); await run({ command: 'revise', args: { plan: plan.id, revision: plan.revision, changes } }); };
  const begin = async (scope: Extract<RelinkRequest, { command: 'begin' }>['args']['scope']) => mutate(async () => { const next = await relink(catalog, { command: 'begin', args: { scope } }, 'plan'); setPlan(next); setEpoch(v => v + 1); });
  const attempt = (action: () => Promise<void>) => { if (admission.current) return; admission.current = true; setAdmitting(true); setError(''); void action().catch(e => setError(errorText(e))).finally(() => { admission.current = false; setAdmitting(false); }); };
  const observed = selected && original && JSON.stringify(selected.key) === JSON.stringify(original.key) ? original : null;
  if (!open) return null;
  return <Dialog title="Locate originals" onClose={onClose}><div className="metadata-panel"><p>Review changed folder paths or reconnect external storage. Saved reviews require an explicit preparation and apply action.</p>{error && <ErrorNotice message={error} />}{controller.error && <ErrorNotice message={controller.error} />}{!controller.ready && <button onClick={controller.retry}>Retry storage operation status</button>}
    {operation && <section role="status"><p>{operation.action}: {operation.phase.replaceAll('_', ' ')} · {operation.progress} processed{operation.boundary ? ` · ${operation.boundary}` : ''}</p>{operation.write_hold && <p>Catalog writes and new original rendering are held until this operation finishes. Cached browsing remains available.</p>}{operation.error && <ErrorNotice message={operation.error} />}{!relinkTerminal(operation) && <button onClick={() => void controller.cancel()}>Cancel storage operation</button>}</section>}
    <details><summary>Locate a moved folder</summary><EnteredFolder onChange={setFrom} /><Destinations folder value={destinations} onChange={setDestinations} disabled={busy} /><button disabled={busy || !from || !destinations.length} onClick={() => attempt(() => begin({ scope: 'prefix', value: { from: from!, destinations } }))}>Create folder review</button></details>
    {selected && <details key={JSON.stringify(selected.key)}><summary>Locate {selected.filename}</summary><button disabled={busy} onClick={() => attempt(() => run({ command: 'original', args: { key: selected.key } }))}>Check selected original</button>{observed && <><p>{observed.status.state}: {observed.status.detail}</p><p>{referenceLabel(observed.status.current)}</p>{observed.status.candidate && <p>Observed candidate: {pathLabel(observed.status.candidate)}</p>}</>}<Destinations folder={false} value={files} onChange={setFiles} disabled={busy} /><button disabled={busy || !files.length} onClick={() => attempt(() => begin({ scope: 'asset', value: { asset_id: selected.key.asset_id, destinations: files } }))}>Create selected original review</button>
      {observed?.status.logical_volume && <section><button disabled={busy} onClick={() => attempt(() => run({ command: 'mounts' }))}>Check connected storage</button>{mounts && <>{!mounts.complete && <p>Storage enumeration is incomplete; it does not establish that a drive is offline.</p>}{mounts.issues.map((issue, i) => <p key={i}>{issue.kind}: {issue.detail}{issue.path ? ` · ${pathLabel(issue.path)}` : ''}</p>)}{mounts.rows.map(row => <p key={row.token}>{pathLabel(row.path)} · {row.filesystem} <button disabled={busy || !mounts.complete || mounts.issues.length > 0 || !row.identity_available || !row.token} onClick={() => attempt(() => begin({ scope: 'volume', value: { logical_volume: observed.status.logical_volume!, mount_token: row.token } }))}>Review reconnect to this volume</button></p>)}</>}</section>}
    </details>}
    <LazyDetails title="Saved relink reviews"><ReviewPages key={epoch} catalog={catalog} kind="plans" first="" request={after => ({ command: 'plans', args: { after: after as string, limit: '20' } })}>{row => <><p>{row.state} · {row.summary_complete ? `${row.total} originals` : 'Counts unavailable'}</p><button disabled={busy} onClick={() => attempt(async () => setPlan(await relink(catalog, { command: 'plan', args: { plan: row.id } }, 'plan')))}>Open saved review</button><details><summary>Review identifier</summary>{row.id}</details></>}</ReviewPages></LazyDetails>
    {plan && <PlanReview key={`${plan.id}:${plan.revision}`} catalog={catalog} plan={plan} busy={busy} run={run} revise={revise} />}
  </div></Dialog>;
}

import { useEffect, useRef, useState } from 'react';
import { chooseLocation, command, errorText, imageKey, type GridImage } from '../bridge';
import { photoExport, terminal, type Budgets, type Destination, type ExecutionLimits, type Item, type Job, type Metadata, type NativePath, type Options, type Output, type ProfileAdmission, type TargetKey } from '../photoExport';
import { waitForExport, decimal, budgets as validateBudgets, execution, executionDraft, executionFields, naming, type ExecutionDraft } from '../exportWorkflow';
import { exportOutput, initialExportSettings } from '../exportSettings';
import type { ExportAction, ExportAdmission, usePhotoExport } from '../state/usePhotoExport';
import { Dialog, ErrorNotice } from './Controls';
import { ExportSettings } from './ExportSettings';
import { ExportPage, ExportPath, ExportPlan, ExportProgress } from './ExportReview';

type Controller = ReturnType<typeof usePhotoExport>;
export type ExportGate = <T>(action: () => Promise<T>) => Promise<T>;
type Frozen = { target: TargetKey; metadata: Metadata; filename: string; label: string };
type Review = { token: string; total: string; targets: Frozen[]; output: Output; budgets: Budgets };
type Token = { kind: 'profile'; profile: ProfileAdmission } | { kind: 'destinations'; token: string };
export type ImportedPathPreparation = { phase: 'required' | 'pending' | 'complete' | 'unbound'; diagnostic?: string };
const pendingPathProjection = 'export alias index has pending path projections; run bounded reconciliation first';
const tokenId = (v: Token) => v.kind === 'profile' ? v.profile.token : v.token;

export const requiresImportedPathPreparation = (message: string) => message.includes(pendingPathProjection);

export function ImportedPathPreparationControls({ state, progress, rows, uncertain, locked, busy, confirm, prepare, setRows }: {
  state: ImportedPathPreparation | null; progress: string; uncertain: boolean; locked: boolean; busy: boolean;
  rows: string; confirm: (() => void) | null; prepare: () => void; setRows: (value: string) => void;
}) {
  const message = state?.phase === 'required'
    ? 'Imported photo locations need preparation before this reviewed output can be added to the saved job.'
    : state?.phase === 'pending'
      ? 'More imported photo locations need preparation. Prepare another bounded step.'
      : state?.phase === 'unbound'
        ? 'Some imported photos do not have a current location. Close Export, use Locate originals, then return and prepare locations again.'
        : state?.phase === 'complete'
          ? 'Imported photo locations are prepared. Retry Append this reviewed output explicitly; LensWorks will not retry it automatically.'
          : 'Imported photos require one-time location preparation before their first output is appended.';
  const needsConfirmation = !!state && uncertain;
  return <details open={!!state}><summary>Imported photo locations {state ? '— action required' : '— prepare before first append'}</summary>
    {state?.phase === 'required' ? <ErrorNotice message={message} /> : <p role={state ? 'status' : undefined}>{message}</p>}
    {state?.diagnostic && <details><summary>Technical error details</summary><p>{state.diagnostic}</p></details>}
    {needsConfirmation && <><p>First confirm the saved job state. This checks what was stored and does not replay the rejected append.</p><button disabled={busy || !confirm} onClick={confirm ?? undefined}>Confirm saved job state</button></>}
    {!needsConfirmation && !['complete', 'unbound'].includes(state?.phase ?? '') && <><label>Paths per step<input value={rows} onChange={event => setRows(event.target.value)} /></label><button disabled={locked} onClick={prepare}>Prepare one path step</button></>}
    {progress && <p role="status">{progress}</p>}
  </details>;
}

export function ExportPanel({ catalog, open, rows, controller, gate: appGate, blocked, onDirectPending, onClose }: {
  catalog: string; open: boolean; rows: GridImage[]; controller: Controller; gate: ExportGate; blocked: boolean; onDirectPending: (pending: boolean) => void; onClose: () => void;
}) {
  const [options, setOptions] = useState<Options | null>(null), [settings, setSettings] = useState(initialExportSettings);
  const [budget, setBudget] = useState<Budgets | null>(null), [limits, setLimits] = useState<ExecutionDraft | null>(null), [override, setOverride] = useState(false);
  const [optionEpoch, setOptionEpoch] = useState(0), [epoch, setEpoch] = useState(0), [error, setError] = useState(''), [busy, setBusy] = useState('');
  const [job, setJob] = useState<Job | null>(null), [inspect, setInspect] = useState<Item | null>(null), [showJobs, setShowJobs] = useState(false);
  const [chosen, setChosen] = useState(new Set<string>()), [frozen, setFrozen] = useState<Frozen[]>([]), [review, setReview] = useState<Review | null>(null);
  const [directory, setDirectory] = useState<NativePath | null>(null), [prefix, setPrefix] = useState(''), [suffix, setSuffix] = useState(''), [variantSuffix, setVariantSuffix] = useState(true), [sequence, setSequence] = useState('');
  const [metadataMode, setMetadataMode] = useState<'omit' | 'resolved'>('omit'), [baseModel, setBaseModel] = useState('');
  const [tokens, setTokens] = useState<Token[]>([]), [overwrite, setOverwrite] = useState(new Set<string>()), [appended, setAppended] = useState(new Set<string>());
  const [directPending, setDirectPending] = useState(false);
  const pendingCallback = useRef(onDirectPending); pendingCallback.current = onDirectPending;
  const [uncertain, setUncertain] = useState(false), [recoverAck, setRecoverAck] = useState(false), [recovery, setRecovery] = useState<string | null>(null);
  const [recoveredKey, setRecoveredKey] = useState<string | null>(null), [pathPreparation, setPathPreparation] = useState<ImportedPathPreparation | null>(null);
  const pendingRecovery = useRef<string | null>(null);
  const [recoverDirs, setRecoverDirs] = useState('256'), [pathRows, setPathRows] = useState('256'), [paths, setPaths] = useState(''), [runItems, setRunItems] = useState('100'), [runSeconds, setRunSeconds] = useState('300');
  const [pageLimit, setPageLimit] = useState('20');
  const owner = useRef({ alive: true, open, generation: 0 });
  if (owner.current.open !== open) { owner.current.open = open; owner.current.generation += 1; }
  const activeAbort = useRef<AbortController | null>(null);
  const active = useRef(false), reading = useRef<AbortController | null>(null);
  const draftKey = JSON.stringify({ settings, budget, frozen, directory, prefix, suffix, variantSuffix, sequence, metadataMode, baseModel, chosen: [...chosen] });
  const draftKeyRef = useRef(draftKey); draftKeyRef.current = draftKey;
  const pendingReview = useRef<{ generation: number; draftKey: string; value: Omit<Review, 'token' | 'total'> } | null>(null);
  const [availableReview, setAvailableReview] = useState<(Review & { draftKey: string }) | null>(null);
  const adoptableReview = availableReview?.draftKey === draftKey ? availableReview : null;
  useEffect(() => {
    setAvailableReview(value => value?.draftKey === draftKey ? value : null);
    if (pendingReview.current?.draftKey !== draftKey) pendingReview.current = null;
  }, [draftKey]);
  useEffect(() => { owner.current.alive = true; return () => { owner.current.alive = false; activeAbort.current?.abort(); reading.current?.abort(); }; }, []);
  useEffect(() => { if (!open) { activeAbort.current?.abort(); reading.current?.abort(); setPathPreparation(null); setPaths(''); } }, [open]);
  useEffect(() => {
    const abort = new AbortController();
    void photoExport(catalog, { command: 'options' }, 'options', abort.signal).then(value => { if (!abort.signal.aborted) { setOptions(value); setBudget(v => v ?? value.budgets); setLimits(v => v ?? executionDraft(value.execution)); setPageLimit(BigInt(value.page_rows) < 20n ? value.page_rows : '20'); } }).catch(e => { if (!abort.signal.aborted) setError(errorText(e)); });
    return () => abort.abort();
  }, [catalog, optionEpoch]);
  useEffect(() => {
    const op = controller.operation;
    if (!op || !terminal(op)) return;
    if (op.job) setJob(value => value?.id === op.job!.id ? op.job : value);
    if (op.result?.kind === 'profile') {
      const profile = op.result.value;
      setTokens(v => v.some(t => tokenId(t) === profile.token) ? v : [...v, { kind: 'profile', profile }]);
    }
    if (op.result?.kind === 'destinations') {
      const { token, total } = op.result.value;
      setTokens(v => v.some(t => tokenId(t) === token) ? v : [...v, { kind: 'destinations', token }]);
      if (pendingReview.current?.generation === owner.current.generation && pendingReview.current.draftKey === draftKeyRef.current) setAvailableReview({ ...pendingReview.current.value, draftKey: pendingReview.current.draftKey, token, total });
    }
    if (op.result?.kind === 'recovery') { setRecoveredKey(op.result.value.complete ? pendingRecovery.current : null); setRecovery(op.result.value.complete ? 'Recovery completed for this service session. Run remains explicit.' : `Recovery examined its budget (${op.result.value.fenced} fenced). Run recovery again explicitly to finish.`); }
    setEpoch(v => v + 1);
  }, [controller.operation]);
  useEffect(() => { setAppended(new Set()); setInspect(null); }, [job?.id]);
  const pageIdentity = JSON.stringify(rows.map(row => [row.image_id, row.metadata_revision, imageKey(row.key)]));
  useEffect(() => setChosen(new Set()), [pageIdentity]);
  const locked = !!busy || directPending || uncertain || blocked || controller.busy || !options;
  const limitsValue = (): ExecutionLimits | null => override ? execution(limits!, options!) : null;
  let executionError = '', executionKey = '';
  if (options && limits) { try { executionKey = JSON.stringify(limitsValue() ?? options.execution); } catch (e) { executionError = errorText(e); } }
  const currentView = (generation: number) => owner.current.alive && owner.current.open && owner.current.generation === generation;
  const check = (generation: number) => { if (!currentView(generation) || activeAbort.current?.signal.aborted) throw new Error('Export dialog or catalog changed. Review saved work after reopening.'); };
  const gate: ExportGate = action => {
    const generation = owner.current.generation, signal = activeAbort.current!.signal;
    return appGate(async () => { check(generation); return waitForExport(action(), signal); });
  };
  const attempt = (label: string, action: (generation: number) => Promise<void>, writes = false) => {
    if (active.current) return;
    const abort = new AbortController(); activeAbort.current = abort;
    active.current = true; const generation = ++owner.current.generation; setBusy(label); setError('');
    void waitForExport(Promise.resolve().then(() => action(generation)), abort.signal).catch(e => {
      if (currentView(generation)) { const message = errorText(e); setError(message); if (requiresImportedPathPreparation(message)) setPathPreparation({ phase: 'required', diagnostic: message }); if (writes) { setUncertain(true); setShowJobs(true); } }
    }).finally(() => { if (activeAbort.current === abort) { active.current = false; reading.current = null; if (owner.current.alive) setBusy(''); } });
  };
  // Job/Jobs reads have higher actor priority than these writes. They cannot
  // settle an abandoned invocation. Only its successful acknowledgement (or
  // closing this catalog session) releases this independent write hold.
  const direct = (request: { command: 'begin' } | { command: 'seal'; args: { job: string; expected_total: string } }) => {
    setDirectPending(true); pendingCallback.current(true);
    return photoExport(catalog, request, 'job').then(value => {
      if (owner.current.alive) { setDirectPending(false); pendingCallback.current(false); }
      return value;
    });
  };
  const operation = async (request: ExportAction, generation: number) => {
    const admitted = await gate(async () => { check(generation); return controller.admit(request); });
    // Completion is deliberately outside App's gate: Close, Cancel and Yield
    // remain available during hashing, worker execution and publication.
    const result = await waitForExport(admitted.completion, activeAbort.current!.signal);
    check(generation);
    if (result.phase !== 'complete') throw new Error(result.error ?? `Operation ${result.phase}. Inspect saved results before continuing.`);
    return result;
  };
  const readJob = async (id: string, generation: number) => {
    const value = await photoExport(catalog, { command: 'job', args: { job: id } }, 'job');
    check(generation); setJob(value); setInspect(null); setUncertain(false); setEpoch(v => v + 1);
  };
  const release = async (token: Token, generation: number) => {
    await photoExport(catalog, token.kind === 'profile' ? { command: 'profile_release', args: { token: token.profile.token } } : { command: 'result_release', args: { token: token.token } }, 'released');
    check(generation);
    setTokens(v => v.filter(t => tokenId(t) !== tokenId(token)));
    if (token.kind === 'profile') setSettings(v => v.icc?.token === token.profile.token ? { ...v, icc: null } : v);
    else { setReview(v => v?.token === token.token ? null : v); setAvailableReview(v => v?.token === token.token ? null : v); }
    check(generation);
  };
  const chooseProfile = () => attempt('Reading ICC profile', async generation => {
    const choice = await chooseLocation('export_profile'); check(generation); if (!choice) return;
    const result = await operation({ command: 'profile', args: { path: choice.path } }, generation);
    if (result.result?.kind !== 'profile') throw new Error('Profile result missing; inspect operation status.');
    if (currentView(generation)) setSettings(v => ({ ...v, profile: 'icc', icc: result.result!.kind === 'profile' ? result.result!.value : null }));
  });
  const freeze = () => attempt('Freezing selected revisions', async generation => {
    const selected = rows.filter(row => chosen.has(imageKey(row.key))).slice(0, 100);
    if (!selected.length) throw new Error('Choose targets from the displayed library page.');
    const mode = metadataMode, base = baseModel === '' ? null : decimal(baseModel, 'Metadata base model', '1', '9223372036854775807');
    const abort = new AbortController(); reading.current = abort;
    const captured = await gate(async () => {
      check(generation); const targets: Frozen[] = [];
      for (const row of selected) {
        const variant = await waitForExport(command({ command: 'variant', args: { catalog, key: row.key } }, 'variant', abort.signal), abort.signal);
        check(generation);
        const image = await waitForExport(command({ command: 'image', args: { catalog, key: row.key } }, 'image', abort.signal), abort.signal);
        check(generation);
        if (mode === 'resolved' && image.metadata_pending) throw new Error(`Metadata is still preparing for ${image.filename}.`);
        targets.push({ target: { key: variant.key, expected_revision: variant.revision }, filename: image.filename, label: variant.label, metadata: mode === 'omit' ? { mode } : { mode, expected_revision: image.metadata_revision, base_model: base } });
      }
      return targets;
    });
    check(generation); setFrozen(captured); setReview(null); setAvailableReview(null); setAppended(new Set());
  });
  const previewNames = () => attempt('Reviewing destination names', async generation => {
    if (!directory || !frozen.length || !budget) throw new Error('Choose an output directory and freeze targets first.');
    const output = exportOutput(settings) as Output;
    const frozenBudget = structuredClone(validateBudgets(budget));
    const value = { targets: frozen, output, budgets: frozenBudget };
    const requestedDraftKey = draftKey;
    pendingReview.current = { generation, draftKey: requestedDraftKey, value };
    const result = await operation({ command: 'destinations', args: { directory, targets: frozen.map(v => v.target), format: output.format, naming: naming(prefix, suffix, variantSuffix, sequence, frozen.length) } }, generation);
    if (result.result?.kind !== 'destinations') throw new Error('Destination result missing. Inspect operation status.');
    check(generation);
    if (draftKeyRef.current !== requestedDraftKey) throw new Error('Export settings or targets changed. Preview destination names again.');
    setReview({ ...value, ...result.result.value }); setOverwrite(new Set()); setAppended(new Set());
  });
  const appendDestination = (destination: Destination) => attempt('Appending frozen output plan', async generation => {
    if (!job || !review || !destination.destination) return;
    const target = review.targets.find(v => imageKey(v.target.key) === imageKey(destination.target.key) && v.target.expected_revision === destination.target.expected_revision);
    if (!target) throw new Error('Destination does not match this frozen target review.');
    const result = await operation({ command: 'append', args: { job: job.id, expected_total: job.total, target: { ...target.target, destination: destination.destination, overwrite: overwrite.has(imageKey(target.target.key)), metadata: target.metadata }, output: review.output, budgets: review.budgets } }, generation);
    if (result.result?.kind !== 'appended') throw new Error('Append result missing. Refresh the saved job before retrying.');
    check(generation); setJob(result.result.value.job); setPathPreparation(null); setAppended(v => new Set(v).add(imageKey(target.target.key))); setEpoch(v => v + 1);
  }, true);
  const prepareImportedPaths = () => attempt('Preparing imported paths', async generation => {
    const result = await operation({ command: 'paths', args: { limit: decimal(pathRows, 'Paths per step', '1', '512') } }, generation);
    if (result.result?.kind !== 'paths') throw new Error('Imported path preparation result missing. Inspect operation status.');
    check(generation); const value = result.result.value;
    setPaths(`${value.projected} projected; ${value.pending ? 'more steps pending' : 'projection complete'}; ${value.unbound} unbound.`);
    setPathPreparation(current => ({ phase: BigInt(value.unbound) > 0n ? 'unbound' : value.pending ? 'pending' : 'complete', diagnostic: current?.diagnostic }));
  }, true);
  const authorityAction = (kind: 'retry_seal' | 'restore', item: Item) => attempt(kind === 'restore' ? 'Restoring saved publication' : 'Retrying saved seal', async generation => {
    if (!job) return;
    await operation({ command: kind, args: { job: job.id, sequence: item.sequence, authority: item.authority } }, generation);
    await readJob(job.id, generation);
  }, true);
  const stop = (yielding: boolean) => { void (yielding ? controller.yield() : controller.cancel()).catch(e => { if (owner.current.alive) setError(errorText(e)); }); };
  if (!open) return null;
  return <Dialog title="Export photos" onClose={onClose}><div className="metadata-panel export-panel">
    <p>Create JPEG, PNG or TIFF outputs from frozen saved edits. Original source photos are never export destinations. Saved jobs remain available after closing this panel or catalog.</p>
    {error && error !== pathPreparation?.diagnostic && <ErrorNotice message={error} />}{controller.error && controller.error !== pathPreparation?.diagnostic && <ErrorNotice message={controller.error} />}
    {!options && <button onClick={() => setOptionEpoch(v => v + 1)}>Retry export options</button>}{!controller.ready && <button onClick={controller.retry}>Retry export status</button>}
    {busy && <p role="status">{busy}… <button onClick={() => { activeAbort.current?.abort(); reading.current?.abort(); }}>{reading.current ? 'Cancel target preparation' : 'Stop waiting'}</button></p>}
    {directPending && <p role="alert">A saved-job write has no acknowledgement yet. Inspection does not settle it. Catalog writes remain held until its acknowledgement arrives, or you close and reopen the catalog. Stop waiting only releases this dialog; it does not prove cancellation.</p>}
    {blocked && <p role="status">Other catalog work holds export admission. Saved inspection remains available.</p>}
    {uncertain && !job && <button disabled={!controller.ready || controller.busy || !!busy} onClick={() => attempt('Checking saved jobs', async generation => { await photoExport(catalog, { command: 'jobs', args: { after: '0', limit: pageLimit } }, 'jobs'); check(generation); setShowJobs(true); setUncertain(false); setEpoch(v => v + 1); })}>Refresh saved jobs for inspection</button>}{uncertain && <p role="alert">The request may have saved work. Inspect its saved job and items before another append, seal or run. An unacknowledged saved-job write additionally requires its acknowledgement or catalog close and reopen. No request is automatically replayed.</p>}
    {controller.operation && <ExportProgress operation={controller.operation} cancel={() => stop(false)} yieldRun={() => stop(true)} />}
    <section><h3>Explicit recovery</h3><p>Recovery can publish previously accepted intents and reconcile interrupted workers. Opening or inspecting a job does not run recovery. Complete it explicitly before Run in this service session; changing worker limits may require recovery again.</p>
      <label>Recovery directory budget<input value={recoverDirs} onChange={e => setRecoverDirs(e.target.value)} inputMode="numeric" /></label><label className="checkbox"><input type="checkbox" checked={recoverAck} onChange={e => setRecoverAck(e.target.checked)} />I authorize recovery of previously accepted publications.</label>
      <button disabled={locked || !recoverAck || !!executionError} onClick={() => attempt('Recovering saved publications', async generation => { const requested = limitsValue(); pendingRecovery.current = JSON.stringify(requested ?? options!.execution); await operation({ command: 'recover', args: { directories: decimal(recoverDirs, 'Recovery directories', '1', '1024'), limits: requested } }, generation); }, true)}>Recover saved publications</button>{recovery && <p role="status">{recovery}</p>}
    </section>
    {options && budget && limits && <details><summary>Plan budgets and worker limits</summary><p>Defaults are reported by this host. Increasing a planning budget does not increase worker capacity or promise a measured memory ceiling.</p>
      <fieldset disabled={locked || !!review}><legend>Per-item plan budgets</legend><label>Maximum existing destination bytes<input value={budget.max_original_bytes} onChange={e => setBudget({ ...budget, max_original_bytes: e.target.value })} /></label><label>Maximum encoded payload bytes<input value={budget.max_payload_bytes} onChange={e => setBudget({ ...budget, max_payload_bytes: e.target.value })} /></label>{(['directories', 'candidates'] as const).map(key => <label key={key}>Alias {key}<input value={budget.alias_limits[key]} onChange={e => setBudget({ ...budget, alias_limits: { ...budget.alias_limits, [key]: e.target.value } })} /></label>)}</fieldset>
      <label className="checkbox"><input type="checkbox" checked={override} disabled={locked} onChange={e => setOverride(e.target.checked)} />Override all execution limits</label>{override && <fieldset disabled={locked}><legend>Execution limits</legend>{executionFields.map(([key, label]) => <label key={key}>{label}<input value={limits[key]} onChange={e => setLimits({ ...limits, [key]: e.target.value })} inputMode="numeric" /></label>)}<button onClick={() => setLimits(executionDraft(options.execution))}>Reset host execution defaults</button></fieldset>}
      {executionError && <ErrorNotice message={executionError} />}<details><summary>Current host options and page/token bounds</summary><pre>{JSON.stringify(options, null, 2)}</pre></details>
    </details>}
    <section><h3>Saved job</h3><button disabled={locked || uncertain} onClick={() => attempt('Creating saved job', async generation => { const value = await gate(async () => { check(generation); return direct({ command: 'begin' }); }); check(generation); setJob(value); setInspect(null); setEpoch(v => v + 1); }, true)}>Create new saved export job</button>
      {job && <><p><strong>Job {job.sequence}: {job.state}</strong> · {job.completed} of {job.total} processed</p><code>{job.id}</code><button disabled={!!busy} onClick={() => attempt('Refreshing saved job', generation => readJob(job.id, generation))}>Refresh saved job</button>
        {job.state === 'building' && <><p>Append targets from any library page, inspect saved plans, then seal this exact total.</p><button disabled={locked || uncertain || job.total === '0'} onClick={() => attempt('Sealing saved target list', async generation => { await gate(async () => { check(generation); await direct({ command: 'seal', args: { job: job.id, expected_total: job.total } }); check(generation); }); await readJob(job.id, generation); }, true)}>Seal {job.total} saved items</button></>}
        {job.state === 'queued' && <>{recoveredKey !== executionKey && <p>Complete explicit recovery with these execution limits before starting this job.</p>}<label>Attempts in this run<input value={runItems} onChange={e => setRunItems(e.target.value)} /></label><label>Seconds in this run<input value={runSeconds} onChange={e => setRunSeconds(e.target.value)} /></label><button className="primary" disabled={locked || uncertain || recoveredKey !== executionKey || !!executionError} onClick={() => attempt('Running saved outputs', async generation => { await operation({ command: 'run', args: { job: job.id, limits: limitsValue(), max_items: decimal(runItems, 'Run attempts', '1', '1000000'), max_seconds: decimal(runSeconds, 'Run seconds', '1', '86400') } }, generation); await readJob(job.id, generation); }, true)}>Run or continue saved job</button></>}
        {['building', 'queued'].includes(job.state) && <button disabled={locked} onClick={() => attempt('Canceling inactive saved job', async generation => { const admitted: ExportAdmission = await gate(async () => { check(generation); return controller.cancelJob(job.id); }); await waitForExport(admitted.completion, activeAbort.current!.signal); check(generation); await readJob(job.id, generation); }, true)}>Cancel remaining saved items</button>}
      </>}
    </section>
    <ImportedPathPreparationControls state={pathPreparation} progress={paths} rows={pathRows} uncertain={uncertain} locked={locked} busy={!!busy} confirm={job ? () => attempt('Confirming saved job state', generation => readJob(job.id, generation)) : null} prepare={prepareImportedPaths} setRows={setPathRows} />
    {job?.state === 'building' && <section><h3>Prepare targets from the library page</h3><p>Close this panel to browse another page; the saved job stays available. A frozen review holds at most 100 logical images with exact edit and metadata revisions.</p>
      <fieldset disabled={locked || !!review}><legend>Targets</legend><button onClick={() => setChosen(new Set(rows.slice(0, 100).map(row => imageKey(row.key))))}>Select this page</button><button onClick={() => setChosen(new Set())}>Clear selection</button>{rows.slice(0, 100).map(row => <label className="checkbox" key={imageKey(row.key)}><input type="checkbox" checked={chosen.has(imageKey(row.key))} onChange={e => setChosen(v => { const next = new Set(v); if (e.target.checked) next.add(imageKey(row.key)); else next.delete(imageKey(row.key)); return next; })} />{row.filename} · {row.key.variant_id}</label>)}
        <label>Metadata in outputs<select value={metadataMode} onChange={e => { setMetadataMode(e.target.value as typeof metadataMode); setFrozen([]); setAvailableReview(null); }}><option value="omit">Omit metadata</option><option value="resolved">Use resolved metadata at the captured revision</option></select></label>{metadataMode === 'resolved' && <label>Optional retained base-model ID<input value={baseModel} onChange={e => { setBaseModel(e.target.value); setFrozen([]); setAvailableReview(null); }} placeholder="Default resolved base" inputMode="numeric" /></label>}
        <button disabled={!chosen.size} onClick={freeze}>Freeze selected edit and metadata revisions</button>
      </fieldset>
      {!!frozen.length && <details><summary>{frozen.length} frozen logical targets</summary>{frozen.map(v => <p key={imageKey(v.target.key)}>{v.filename} · {v.label} · edit {v.target.expected_revision} · {v.metadata.mode === 'omit' ? 'metadata omitted' : `metadata ${v.metadata.expected_revision}, base ${v.metadata.base_model ?? 'default'}`}</p>)}</details>}
      <ExportSettings value={settings} onChange={setSettings} disabled={locked || !!review} choosingProfile={busy === 'Reading ICC profile'} chooseProfile={chooseProfile} />
      <fieldset disabled={locked || !!review}><legend>Destination naming</legend><button onClick={() => attempt('Choosing output directory', async generation => { const choice = await chooseLocation('export_directory'); check(generation); if (choice) setDirectory(choice.path); })}>Choose output directory</button>{directory && <ExportPath path={directory} />}<label>Prefix<input value={prefix} onChange={e => setPrefix(e.target.value)} /></label><label>Suffix<input value={suffix} onChange={e => setSuffix(e.target.value)} /></label><label className="checkbox"><input type="checkbox" checked={variantSuffix} onChange={e => setVariantSuffix(e.target.checked)} />Include the variant label in each filename</label><label>Optional starting filename sequence<input value={sequence} onChange={e => setSequence(e.target.value)} inputMode="numeric" /></label><p>No existence-based suffix or silent overwrite is added. Review the returned native paths and errors before each append.</p><button disabled={!frozen.length || !directory} onClick={previewNames}>Preview exact destination names</button></fieldset>
      {adoptableReview && !review && <button disabled={locked} onClick={() => { if (adoptableReview.draftKey !== draftKeyRef.current) return; setReview(adoptableReview); setAvailableReview(null); setOverwrite(new Set()); setAppended(new Set()); }}>Use completed naming review ({adoptableReview.total} targets)</button>}
      {review && <section><h4>Frozen destination review · {review.total} targets</h4><p>Each append freezes the reviewed output settings. Existing destinations require explicit authorization; collisions and changed identities reject. To change settings, release this naming review below.</p><ExportPage key={review.token} catalog={catalog} kind="destinations" owner={review.token} limit={pageLimit}>{row => <><strong>{row.name.filename} · {row.name.variant_label}</strong>{row.destination && <ExportPath path={row.destination} />}{row.error && <ErrorNotice message={row.error} />}<label className="checkbox"><input disabled={locked || appended.has(imageKey(row.target.key))} type="checkbox" checked={overwrite.has(imageKey(row.target.key))} onChange={e => setOverwrite(v => { const next = new Set(v); if (e.target.checked) next.add(imageKey(row.target.key)); else next.delete(imageKey(row.target.key)); return next; })} />Authorize replacement of this destination if it already exists, preserving its captured original</label><button disabled={locked || uncertain || !!row.error || !row.destination || appended.has(imageKey(row.target.key))} onClick={() => appendDestination(row)}>{appended.has(imageKey(row.target.key)) ? 'Appended to saved job' : 'Append this reviewed output'}</button></>}</ExportPage></section>}
    </section>}
    {!!tokens.length && <section><h3>Session profiles and naming reviews</h3><p>Tokens remain available until explicitly released or the catalog closes. Saved plans own their bytes independently.</p>{tokens.map(token => <article key={tokenId(token)}>{token.kind === 'profile' ? <><p>{token.profile.name} · {token.profile.bytes} bytes · {token.profile.blake3}</p><button disabled={locked || !!review} onClick={() => setSettings(v => ({ ...v, profile: 'icc', icc: token.profile }))}>Use this ICC profile</button></> : <p>Naming review {token.token}</p>}<button disabled={!!busy} onClick={() => attempt('Releasing session token', generation => release(token, generation))}>Release {token.kind === 'profile' ? 'ICC profile' : 'naming review'}</button></article>)}</section>}
    <details open={showJobs} onToggle={e => setShowJobs(e.currentTarget.open)}><summary>Saved export jobs</summary>{showJobs && <ExportPage key={`jobs:${epoch}`} catalog={catalog} kind="jobs" limit={pageLimit}>{row => <><strong>Job {row.sequence}: {row.state}</strong><p>{row.completed} of {row.total} processed</p><button disabled={!!busy} onClick={() => attempt('Inspecting saved job', generation => readJob(row.id, generation))}>Inspect job {row.sequence}</button></>}</ExportPage>}</details>
    {job && <section><h3>Saved item outcomes</h3><ExportPage key={`${job.id}:${epoch}`} catalog={catalog} kind="items" owner={job.id} limit={pageLimit}>{row => <><strong>{row.name.filename} · {row.name.variant_label}</strong><p>Item {row.sequence}: {row.state}</p><ExportPath path={row.destination} />{row.error && <ErrorNotice message={row.error} />}{row.receipt && <p>{row.receipt.state}: {row.receipt.detail}</p>}<button onClick={() => setInspect(row)}>Review plan and recovery for item {row.sequence}</button></>}</ExportPage>{inspect && <ExportPlan key={`${job.id}:${inspect.sequence}:${epoch}`} catalog={catalog} job={job.id} item={inspect} locked={locked || uncertain} act={authorityAction} />}</section>}
  </div></Dialog>;
}

import { useEffect, useRef, useState } from 'react';
import { command, errorText, imageKey, type GridImage, type Variant } from '../bridge';
import { adjustmentGroups, chosenCopyTargets, copyCandidates, type AdjustmentGroup } from '../copySelection';
import { copyTerminal, editCopy, type CopyData, type CopyInspection, type CopyRequest, type CopyTarget } from '../editCopy';
import type { useEditCopy } from '../state/useEditCopy';
import { Dialog, ErrorNotice } from './Controls';
import './metadata.css';

type Controller = ReturnType<typeof useEditCopy>;
type Mutation = (action: () => Promise<void>) => Promise<void>;

function CopyPages<K extends 'jobs' | 'items'>({ catalog, kind, job, children }: { catalog: string; kind: K; job?: string; children: (row: CopyData[K]['rows'][number]) => React.ReactNode }) {
  const [after, setAfter] = useState('0'), [previous, setPrevious] = useState<string[]>([]);
  const [page, setPage] = useState<CopyData[K] | null>(null), [error, setError] = useState('');
  const [epoch, setEpoch] = useState(0);
  useEffect(() => {
    const abort = new AbortController(); setPage(null); setError('');
    const request: CopyRequest = kind === 'jobs' ? { command: 'jobs', args: { after, limit: '20' } } : { command: 'items', args: { job: job!, after, limit: '20' } };
    void editCopy(catalog, request, kind, abort.signal).then(value => { if (!abort.signal.aborted) setPage(value); }).catch(e => { if (!abort.signal.aborted) setError(errorText(e)); });
    return () => abort.abort();
  }, [catalog, kind, job, after, epoch]);
  return <section>{error && <><ErrorNotice message={error} /><button onClick={() => setEpoch(v => v + 1)}>Retry loading {kind === 'jobs' ? 'batches' : 'results'}</button></>}
    {!page && !error && <p role="status">Loading {kind === 'jobs' ? 'batches' : 'results'}…</p>}
    {page?.rows.map(row => <article key={row.sequence}>{children(row)}</article>)}
    {page && !page.rows.length && <p>{page.next ? 'More entries are available on the next page.' : 'No entries on this page.'}</p>}
    <div className="button-group"><button disabled={!previous.length} onClick={() => { setAfter(previous.at(-1)!); setPrevious(v => v.slice(0, -1)); }}>Previous {kind === 'jobs' ? 'batches' : 'results'}</button><button disabled={!page?.next} onClick={() => { setPrevious(v => [...v.slice(-7), after]); setAfter(page!.next!); }}>Next {kind === 'jobs' ? 'batches' : 'results'}</button><button onClick={() => setEpoch(v => v + 1)}>Refresh {kind === 'jobs' ? 'batches' : 'results'}</button></div>
  </section>;
}

export function CopyPanel({ catalog, open, selected, rows, source, controller, mutate, jobsHeld, writeHeld, onClose }: {
  catalog: string; open: boolean; selected: GridImage | null; rows: GridImage[]; source: () => Variant | null;
  controller: Controller; mutate: Mutation; jobsHeld: boolean; writeHeld: boolean; onClose: () => void;
}) {
  const [groups, setGroups] = useState<AdjustmentGroup[]>(['exposure', 'white_balance', 'tone', 'color']);
  const [inspection, setInspection] = useState<CopyInspection | null>(null);
  const [chosen, setChosen] = useState(new Set<string>()), [showJobs, setShowJobs] = useState(false);
  const [busy, setBusy] = useState(''), [error, setError] = useState(''), [uncertain, setUncertain] = useState(false), [epoch, setEpoch] = useState(0);
  const admission = useRef(false), mounted = useRef(true), reading = useRef<AbortController | null>(null);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; reading.current?.abort(); }; }, []);
  const job = controller.operation && !copyTerminal(controller.operation) && controller.operation.job.id === inspection?.job.id ? controller.operation.job : inspection?.job;
  useEffect(() => {
    const operation = controller.operation;
    if (!operation || !copyTerminal(operation)) return;
    setInspection(value => value?.job.id === operation.job.id ? { ...value, job: operation.job } : value);
    setEpoch(value => value + 1);
  }, [controller.operation]);
  const candidates = copyCandidates(rows, inspection?.source.key ?? selected?.key ?? null);
  const pageKey = JSON.stringify(candidates.map(row => imageKey(row.key)));
  useEffect(() => setChosen(new Set()), [pageKey]);
  const locked = !!busy || writeHeld || !controller.ready || controller.busy;
  const attempt = (label: string, action: () => Promise<void>, writes = false) => {
    if (admission.current) return;
    admission.current = true; setBusy(label); setError('');
    void (async () => {
      try { if (writes) await mutate(action); else await action(); }
      catch (e) { if (mounted.current) { setError(errorText(e)); if (writes) { setUncertain(true); setShowJobs(true); } } }
      finally { admission.current = false; reading.current = null; if (mounted.current) setBusy(''); }
    })();
  };
  const inspect = async (id: string) => {
    const result = await editCopy(catalog, { command: 'inspect', args: { job: id } }, 'inspection');
    if (mounted.current) { setInspection(result); setUncertain(false); setChosen(new Set()); setEpoch(v => v + 1); }
  };
  const begin = () => attempt('Saving source settings', async () => {
    const variant = source(); if (!variant) throw new Error('Select a source photo first.');
    const saved = await editCopy(catalog, { command: 'begin', args: { source: variant.key, expected_revision: variant.revision, groups } }, 'job');
    await inspect(saved.id);
  }, true);
  const append = () => attempt('Adding selected targets', async () => {
    if (!job || !inspection) return;
    const targets = chosenCopyTargets(rows, chosen, inspection.source.key);
    if (!targets.length) throw new Error('Choose targets from the displayed page.');
    const abort = new AbortController(); reading.current = abort;
    const captured: CopyTarget[] = [];
    for (const row of targets) {
      const variant = await command({ command: 'variant', args: { catalog, key: row.key } }, 'variant', abort.signal);
      captured.push({ key: variant.key, expected_revision: variant.revision });
    }
    if (abort.signal.aborted) throw new DOMException('Canceled', 'AbortError');
    await editCopy(catalog, { command: 'append', args: { job: job.id, expected_total: job.total, targets: captured } }, 'job', abort.signal);
    await inspect(job.id);
  }, true);
  if (!open) return null;
  return <Dialog title="Copy adjustments" onClose={onClose}><div className="metadata-panel">
    <p>Copy chosen settings to other photos or variants. Each target keeps its other adjustments and its own undo history.</p>
    {error && <ErrorNotice message={error} />}{controller.error && <ErrorNotice message={controller.error} />}
    {!controller.ready && <button onClick={controller.retry}>Retry copy status</button>}
    {busy && <p role="status">{busy}… {reading.current && <button onClick={() => reading.current?.abort()}>Cancel preparation</button>}</p>}
    {writeHeld && <p role="status">Storage work is holding catalog writes. Review is available.</p>}
    {uncertain && <p role="alert">The request may have saved work. Refresh this review or open a saved batch before trying again.</p>}
    {controller.operation && <section aria-label="Adjustment copy progress"><strong>{controller.operation.phase.replaceAll('_', ' ')}</strong><p role="status">{controller.operation.job.completed} of {controller.operation.job.total} targets processed. Check individual results for conflicts or incompatible settings.</p>{controller.operation.error && <ErrorNotice message={controller.operation.error} />}{!copyTerminal(controller.operation) && <button onClick={() => void controller.cancel()}>Cancel adjustment copy</button>}</section>}
    {!inspection ? <section><h3>Source settings</h3><p>{selected?.filename ?? 'Select a photo in the library, then reopen this panel.'}</p>
      <fieldset disabled={locked}><legend>Adjustment groups</legend>{adjustmentGroups.map(([id, label]) => <label className="checkbox" key={id}><input type="checkbox" checked={groups.includes(id)} onChange={e => setGroups(value => e.target.checked ? [...value, id] : value.filter(group => group !== id))} />{label}</label>)}</fieldset>
      <button disabled={locked || uncertain || !selected || !groups.length} onClick={begin}>Use these source settings</button>
    </section> : <section><h3>Saved source</h3><p>{inspection.name.available ? inspection.name.filename : 'Source photo unavailable'}{inspection.name.variant_label && ` · ${inspection.name.variant_label}`} · Revision {inspection.source.expected_revision}</p>
      <p>{inspection.groups.map(group => adjustmentGroups.find(([id]) => id === group)?.[1] ?? group).join(', ')}</p><p className="hint">This batch uses the settings saved when it was created, even if the source changes later.</p>
      <details><summary>Saved recipe and source identity</summary><pre>{JSON.stringify({ source: inspection.source, digest: inspection.digest, recipe: inspection.recipe }, null, 2)}</pre></details>
      <p><strong>{job?.state}</strong> · {job?.total} targets · {job?.completed} processed</p>
      <div className="button-group"><button disabled={!!busy} onClick={() => attempt('Refreshing review', () => inspect(inspection.job.id))}>Refresh saved review</button><button disabled={locked} onClick={() => { setInspection(null); setUncertain(false); }}>Start another batch</button></div>
      {job?.state === 'building' && <section><h3>Add targets from this page</h3><p>These are the {candidates.length} eligible photos on the current library page. Close this panel to browse another page; this saved batch stays available.</p>
        <div className="button-group"><button disabled={locked} onClick={() => setChosen(new Set(candidates.map(row => imageKey(row.key))))}>Select this page</button><button disabled={locked} onClick={() => setChosen(new Set())}>Clear target selection</button></div>
        {candidates.map(row => <label className="checkbox" key={imageKey(row.key)}><input disabled={locked} type="checkbox" checked={chosen.has(imageKey(row.key))} onChange={e => setChosen(value => { const next = new Set(value); if (e.target.checked) next.add(imageKey(row.key)); else next.delete(imageKey(row.key)); return next; })} />{row.filename}{row.key.variant_id !== 'master' && <span className="hint"> · variant {row.key.variant_id}</span>}</label>)}
        <button disabled={locked || uncertain || !chosenCopyTargets(rows, chosen, inspection.source.key).length} onClick={append}>Add chosen targets to batch</button>
        <p>Review the saved targets below before sealing. A sealed batch cannot accept more targets.</p><button disabled={locked || uncertain || job.total === '0'} onClick={() => attempt('Sealing target list', async () => { await editCopy(catalog, { command: 'seal', args: { job: job.id, expected_total: job.total } }, 'job'); await inspect(job.id); }, true)}>Seal target list</button>
      </section>}
      {job?.state === 'queued' && <><p>Starting applies the selected groups to {job.total} targets. Changed targets are reported as conflicts. Cancel keeps adjustments already applied.</p>{jobsHeld && <p>Review and release restored jobs in Backups before starting this batch.</p>}<button className="primary" disabled={locked || uncertain || jobsHeld} onClick={() => attempt('Starting adjustment copy', () => controller.run(job.id), true)}>Run saved batch</button></>}
      {job && ['building', 'queued'].includes(job.state) && !controller.busy && <button disabled={locked} onClick={() => attempt('Canceling saved batch', async () => { await editCopy(catalog, { command: 'cancel', args: { job: job.id, operation: null } }, 'job'); await inspect(job.id); }, true)}>Cancel saved batch</button>}
      <CopyPages key={`${inspection.job.id}:${epoch}`} catalog={catalog} kind="items" job={inspection.job.id}>{row => <><strong>{row.name.available ? row.name.filename : 'Target unavailable'}</strong>{row.name.variant_label && ` · ${row.name.variant_label}`}<p>{row.state} · Expected revision {row.target.expected_revision} · Current revision {row.current_revision ?? 'unavailable'}{row.applied_revision !== null && ` · Applied revision ${row.applied_revision}`}</p>{row.error && <p role="status">{row.error}</p>}<details><summary>Target identity</summary><pre>{JSON.stringify(row.target.key, null, 2)}</pre></details></>}</CopyPages>
    </section>}
    <details open={showJobs} onToggle={e => setShowJobs(e.currentTarget.open)}><summary>Saved batches</summary>{showJobs && <CopyPages key={epoch} catalog={catalog} kind="jobs">{row => <><strong>{row.state}</strong><p>{row.completed} of {row.total} processed</p><button disabled={!!busy} onClick={() => attempt('Opening saved batch', () => inspect(row.id))}>Review batch {row.sequence}</button><details><summary>Batch identifier</summary>{row.id}</details></>}</CopyPages>}</details>
  </div></Dialog>;
}

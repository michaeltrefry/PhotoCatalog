import { useEffect, useState, type ReactNode } from 'react';
import { displayPath, errorText } from '../bridge';
import { photoExport, terminal, type Data, type Item, type Operation, type Plan, type PlanChunk, type Request, type NativePath } from '../photoExport';
import { ErrorNotice } from './Controls';

export function ExportPath({ path }: { path: NativePath }) {
  return <><code className="export-path">{displayPath(path)}</code><details><summary>Exact native path</summary><pre>{JSON.stringify(path)}</pre></details></>;
}
export function ExportProgress({ operation, cancel, yieldRun }: { operation: Operation; cancel: () => void; yieldRun: () => void }) {
  return <section aria-label="Photo export progress"><strong>{operation.kind.replaceAll('_', ' ')} · {operation.phase.replaceAll('_', ' ')}</strong><p role="status">{operation.stage === 'draining' ? 'Waiting for the owned worker to stop' : operation.stage.replaceAll('_', ' ')} · {operation.processed} attempts processed{operation.stream_bytes !== null && ` · ${operation.stream_bytes} bytes`}</p>
    {operation.job && <p>Saved job: {operation.job.completed} of {operation.job.total} processed. Inspect item states for successful outputs.</p>}
    {operation.error && <ErrorNotice message={operation.error} />}
    {!terminal(operation) && <div className="button-group"><button onClick={cancel}>Cancel export operation</button>{operation.kind === 'run' && <button onClick={yieldRun}>Pause after stopping worker</button>}</div>}
    {operation.kind === 'run' && terminal(operation) && operation.job?.state === 'queued' && <p>This run stopped with saved work remaining. Review results and explicitly run again to continue.</p>}
    {operation.phase === 'paused' && <p>The worker has stopped. The saved job can be continued with an explicit Run.</p>}
    <details><summary>Operation identity and result</summary><pre>{JSON.stringify(operation, null, 2)}</pre></details>
    {operation.write_hold && <p>Catalog writes are held until the owned operation releases them. Cached browsing and cancellation remain available.</p>}
  </section>;
}
export function ExportPage<K extends 'jobs' | 'items' | 'destinations'>({ catalog, kind, owner, limit, children }: {
  catalog: string; kind: K; owner?: string; limit: string; children: (row: Data[K]['rows'][number]) => ReactNode;
}) {
  const [after, setAfter] = useState('0'), [previous, setPrevious] = useState<string[]>([]), [epoch, setEpoch] = useState(0);
  const [page, setPage] = useState<Data[K] | null>(null), [error, setError] = useState('');
  useEffect(() => {
    const abort = new AbortController(); setPage(null); setError('');
    const request: Request = kind === 'jobs' ? { command: 'jobs', args: { after, limit } } : kind === 'items' ? { command: 'items', args: { job: owner!, after, limit } } : { command: 'destination_rows', args: { token: owner!, after, limit } };
    void photoExport(catalog, request, kind, abort.signal).then(value => { if (!abort.signal.aborted) setPage(value); }).catch(e => { if (!abort.signal.aborted) setError(errorText(e)); });
    return () => abort.abort();
  }, [catalog, kind, owner, limit, after, epoch]);
  const label = kind === 'destinations' ? 'destinations' : kind === 'items' ? 'items' : 'jobs';
  return <section aria-label={`Saved export ${label}`}>
    {error && <><ErrorNotice message={error} /><button onClick={() => setEpoch(v => v + 1)}>Retry loading {label}</button></>}
    {!page && !error && <p role="status">Loading {label}…</p>}
    {page?.rows.map((row, index) => <article key={'sequence' in row ? row.sequence : index}>{children(row)}</article>)}
    {page && !page.rows.length && <p>{page.next !== null ? 'No matching entries in this scan; continue to the next page.' : 'No entries on this page.'}</p>}
    <div className="button-group"><button disabled={!previous.length} onClick={() => { setAfter(previous.at(-1)!); setPrevious(v => v.slice(0, -1)); }}>Previous {label}</button><button disabled={!page || page.next === null} onClick={() => { setPrevious(v => [...v.slice(-7), after]); setAfter(page!.next!); }}>Next {label}</button><button onClick={() => { setAfter('0'); setPrevious([]); setEpoch(v => v + 1); }}>First {label} page</button><button onClick={() => setEpoch(v => v + 1)}>Refresh {label}</button></div>
  </section>;
}
export function ExportPlan({ catalog, item, job, locked, act }: { catalog: string; item: Item; job: string; locked: boolean; act: (kind: 'retry_seal' | 'restore', item: Item) => void }) {
  const [plan, setPlan] = useState<Plan | null>(null), [chunk, setChunk] = useState<PlanChunk | null>(null);
  const [offset, setOffset] = useState('0'), [previous, setPrevious] = useState<string[]>([]), [epoch, setEpoch] = useState(0), [error, setError] = useState('');
  const [acknowledged, setAcknowledged] = useState(false);
  useEffect(() => {
    const abort = new AbortController(); setPlan(null); setError(''); setAcknowledged(false);
    void photoExport(catalog, { command: 'plan', args: { job, sequence: item.sequence } }, 'plan', abort.signal).then(value => { if (!abort.signal.aborted) setPlan(value); }).catch(e => { if (!abort.signal.aborted) setError(errorText(e)); });
    return () => abort.abort();
  }, [catalog, job, item.sequence, item.authority, epoch]);
  useEffect(() => {
    if (!plan) return;
    const abort = new AbortController(); setChunk(null); setError('');
    void photoExport(catalog, { command: 'plan_chunk', args: { job, sequence: item.sequence, authority: plan.authority, offset, bytes: '8192' } }, 'plan_chunk', abort.signal).then(value => { if (!abort.signal.aborted) setChunk(value); }).catch(e => { if (!abort.signal.aborted) setError(errorText(e)); });
    return () => abort.abort();
  }, [catalog, job, item.sequence, plan, offset, epoch]);
  return <section aria-label="Frozen export plan"><h4>Frozen plan · item {item.sequence}</h4>{error && <><ErrorNotice message={error} /><button onClick={() => setEpoch(v => v + 1)}>Retry plan inspection</button></>}
    {!plan && !error && <p role="status">Loading saved authority…</p>}
    {plan && <><p>Original</p><ExportPath path={plan.original} /><p>Destination</p><ExportPath path={plan.destination.destination} /><p>Saved authority: <code>{plan.authority}</code></p><details><summary>Output, source, metadata and destination observations</summary><pre>{JSON.stringify(plan, null, 2)}</pre></details>
      <p>Exact saved plan text, byte offset {offset} of {chunk?.total_bytes ?? plan.plan_bytes}. This text is retained without parsing or rewriting.</p>{chunk ? <pre className="export-plan-text">{chunk.text}</pre> : <p role="status">Loading exact plan chunk…</p>}
      <div className="button-group"><button disabled={!previous.length} onClick={() => { setOffset(previous.at(-1)!); setPrevious(v => v.slice(0, -1)); }}>Previous plan chunk</button><button disabled={!chunk || chunk.next === null} onClick={() => { setPrevious(v => [...v.slice(-7), offset]); setOffset(chunk!.next!); }}>Next plan chunk</button><button onClick={() => { setOffset('0'); setPrevious([]); setEpoch(v => v + 1); }}>First plan chunk</button></div>
      <p>Retry seal only retries saved work; Run remains explicit. Restore may finalize an already accepted publication. It never promises to replace a changed destination.</p>
      <label className="checkbox"><input type="checkbox" checked={acknowledged} onChange={e => setAcknowledged(e.target.checked)} />I reviewed this exact authority and destination.</label><div className="button-group"><button disabled={locked || !acknowledged} onClick={() => act('retry_seal', { ...item, authority: plan.authority })}>Retry saved seal</button><button disabled={locked || !acknowledged} onClick={() => act('restore', { ...item, authority: plan.authority })}>Restore or finalize saved publication</button></div>
    </>}
    {item.receipt && <section><h4>Saved receipt: {item.receipt.state}</h4><p>{item.receipt.detail}</p><ExportPath path={item.receipt.destination} /><p>Recovery directory</p><ExportPath path={item.receipt.recovery_directory} />{item.receipt.captured_original && <><p>Retained original destination</p><ExportPath path={item.receipt.captured_original} /></>}</section>}
  </section>;
}

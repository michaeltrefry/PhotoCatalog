import { useEffect, useRef, useState } from 'react';
import { command, errorText, type GridImage, type SearchOptions } from '../bridge';
import { organize, nonnegativeDecimal, jobName, selectionPage, type Collection, type ImageIdentity, type Job, type JobItem, type Keyword, type KeywordKind, type MembershipCursor, type OrganizationOperation, type Page, type Placement } from '../organization';
import { OrganizationRunner } from '../state/organizationRunner';
import { Dialog, ErrorNotice } from './Controls';
import './organization.css';

function variantName(label: string | undefined | null) { return label == null ? 'Loading variant name…' : label || 'Unnamed variant'; }
function useVariantNames(catalog: string, rows: GridImage[], enabled: boolean) {
  const [names, setNames] = useState<Record<string, string>>({}); const [error, setError] = useState(''); const [loading, setLoading] = useState(false);
  const identity = JSON.stringify(rows.map(row => [row.image_id, row.key]));
  useEffect(() => { const abort = new AbortController(); setNames({}); setError(''); if (!enabled) { setLoading(false); return () => abort.abort(); } setLoading(true);
    void (async () => { const next: Record<string, string> = {}; for (const row of rows) { const variant = await command({ command: 'variant', args: { catalog, key: row.key } }, 'variant', abort.signal); next[row.image_id] = variant.label; } if (!abort.signal.aborted) setNames(next); })().catch(e => { if (!abort.signal.aborted) setError(errorText(e)); }).finally(() => { if (!abort.signal.aborted) setLoading(false); }); return () => abort.abort();
  }, [catalog, identity, enabled]);
  return { names, error, loading };
}

function useNamedCollection(catalog: string, id: string | null, epoch = 0) {
  const [result, setResult] = useState<{ id: string; value: Collection | null } | null>(null); const [error, setError] = useState('');
  useEffect(() => { const abort = new AbortController(); setError(''); if (!id) return () => abort.abort();
    void organize(catalog, { command: 'collection', args: { id } }, 'collection', abort.signal).then(value => setResult({ id, value })).catch(e => { if (!abort.signal.aborted) setError(errorText(e)); }); return () => abort.abort();
  }, [catalog, id, epoch]);
  return { collection: result?.id === id ? result?.value : undefined, error };
}
function jobCollectionId(job: Job | null) { return job && (job.operation.operation === 'add_collection' || job.operation.operation === 'remove_collection') ? job.operation.collection : null; }

type Mutation = (action: () => Promise<void>) => Promise<void>;
function Pager({ next, first, advance, busy }: { next: unknown; first: () => void; advance: () => void; busy: boolean }) {
  return <div className="button-group"><button disabled={busy} onClick={first}>First page / Refresh</button><button disabled={busy || next == null} onClick={advance}>Next page</button></div>;
}
function Evidence({ value }: { value: string }) { return <details><summary>Retained provenance</summary><pre className="organization-evidence">{value}</pre></details>; }
function KeywordBrowser({ catalog, chosen, onChoose, epoch }: { catalog: string; chosen: Keyword | null; onChoose: (row: Keyword) => void; epoch: number }) {
  const [kind, setKind] = useState<KeywordKind>('hierarchical');
  const [parents, setParents] = useState<Keyword[]>([]);
  const [after, setAfter] = useState('0'); const [refresh, setRefresh] = useState(0);
  const [page, setPage] = useState<Page<Keyword, string>>({ rows: [], next: null });
  const [busy, setBusy] = useState(false); const [error, setError] = useState('');
  const parent = parents.at(-1)?.id ?? null;
  useEffect(() => {
    const abort = new AbortController(); setBusy(true); setError(''); setPage({ rows: [], next: null });
    void organize(catalog, { command: 'keywords', args: { kind, parent, after, limit: 40 } }, 'keywords', abort.signal).then(setPage).catch(e => { if (!abort.signal.aborted) setError(errorText(e)); }).finally(() => { if (!abort.signal.aborted) setBusy(false); });
    return () => abort.abort();
  }, [catalog, kind, parent, after, refresh, epoch]);
  return <div><label className="form-field">Keyword type<select value={kind} onChange={e => { setKind(e.target.value as KeywordKind); setParents([]); setAfter('0'); }}><option value="hierarchical">Hierarchical</option><option value="flat">Flat</option></select></label>
    <div className="organization-breadcrumb"><button onClick={() => { setParents([]); setAfter('0'); }}>Root</button>{parents.map((row, index) => <button key={row.id} onClick={() => { setParents(parents.slice(0, index + 1)); setAfter('0'); }}>{row.name}</button>)}</div>
    {error && <ErrorNotice message={error} />}{busy ? <p role="status">Loading keywords…</p> : !page.rows.length && <p>No keywords on this page.</p>}
    <ul className="organization-list">{page.rows.map(row => <li key={row.id}><button aria-pressed={chosen?.id === row.id} onClick={() => onChoose(row)}>{row.name}</button>{kind === 'hierarchical' && <button disabled={parents.length >= 63} aria-label={`Browse children of ${row.name}`} onClick={() => { setParents([...parents, row]); setAfter('0'); }}>Children →</button>}</li>)}</ul>
    <Pager busy={busy} next={page.next} first={() => { setAfter('0'); setRefresh(v => v + 1); }} advance={() => setAfter(page.next!)} />
  </div>;
}
function CollectionBrowser({ catalog, chosen, onChoose, epoch }: { catalog: string; chosen: Collection | null; onChoose: (row: Collection) => void; epoch: number }) {
  const [after, setAfter] = useState(''); const [refresh, setRefresh] = useState(0);
  const [page, setPage] = useState<Page<Collection, string>>({ rows: [], next: null });
  const [busy, setBusy] = useState(false); const [error, setError] = useState('');
  useEffect(() => {
    const abort = new AbortController(); setBusy(true); setError(''); setPage({ rows: [], next: null });
    void organize(catalog, { command: 'collections', args: { after, limit: 40 } }, 'collections', abort.signal).then(setPage).catch(e => { if (!abort.signal.aborted) setError(errorText(e)); }).finally(() => { if (!abort.signal.aborted) setBusy(false); });
    return () => abort.abort();
  }, [catalog, after, refresh, epoch]);
  return <div>{error && <ErrorNotice message={error} />}{busy ? <p role="status">Loading collections…</p> : !page.rows.length && <p>No collections on this page.</p>}
    <ul className="organization-list">{page.rows.map(row => <li key={row.id}><button aria-pressed={chosen?.id === row.id} onClick={() => onChoose(row)}>{row.name}</button><span>Revision {row.revision}</span></li>)}</ul>
    <Pager busy={busy} next={page.next} first={() => { setAfter(''); setRefresh(v => v + 1); }} advance={() => setAfter(page.next!)} />
  </div>;
}
function Synonyms({ catalog, keyword, mutate, epoch }: { catalog: string; keyword: Keyword; mutate: Mutation; epoch: number }) {
  const [after, setAfter] = useState(''); const [name, setName] = useState(''); const [refresh, setRefresh] = useState(0);
  const [page, setPage] = useState<Page<{ synonym: string; provenance_json: string }, string>>({ rows: [], next: null });
  const [busy, setBusy] = useState(false); const [error, setError] = useState('');
  useEffect(() => { const abort = new AbortController(); setBusy(true); setPage({ rows: [], next: null });
    void organize(catalog, { command: 'synonyms', args: { keyword: keyword.id, after, limit: 40 } }, 'synonyms', abort.signal).then(setPage).catch(e => { if (!abort.signal.aborted) setError(errorText(e)); }).finally(() => { if (!abort.signal.aborted) setBusy(false); }); return () => abort.abort();
  }, [catalog, keyword.id, after, refresh, epoch]);
  return <section><h3>Synonyms for {keyword.name}</h3>{error && <ErrorNotice message={error} />}{page.rows.map(row => <div key={row.synonym}>{row.synonym}<Evidence value={row.provenance_json} /></div>)}{!busy && !page.rows.length && <p>No synonyms on this page.</p>}
    <Pager busy={busy} next={page.next} first={() => { setAfter(''); setRefresh(v => v + 1); }} advance={() => setAfter(page.next!)} />
    <form onSubmit={e => { e.preventDefault(); void mutate(async () => { await organize(catalog, { command: 'add_synonym', args: { keyword: keyword.id, synonym: name.trim() } }, 'acknowledged'); setName(''); setAfter(''); setRefresh(v => v + 1); }); }}><label className="form-field">New synonym<input maxLength={1024} value={name} onChange={e => setName(e.target.value)} /></label><button disabled={!name.trim()}>Add synonym</button></form>
  </section>;
}
function Members({ catalog, collection, epoch, onChoose }: { catalog: string; collection: Collection; epoch: number; onChoose: (row: GridImage) => void }) {
  const [after, setAfter] = useState<MembershipCursor | null>(null); const [refresh, setRefresh] = useState(0);
  const [page, setPage] = useState<{ rows: { photo: GridImage; variantLabel: string; position: string; provenance: string }[]; next: MembershipCursor | null; scanned: string }>({ rows: [], next: null, scanned: '0' });
  const [busy, setBusy] = useState(false); const [error, setError] = useState('');
  useEffect(() => { const abort = new AbortController(); setBusy(true); setError(''); setPage({ rows: [], next: null, scanned: '0' });
    void (async () => { const result = await organize(catalog, { command: 'members', args: { collection: collection.id, after, limit: 20 } }, 'members', abort.signal);
      const rows = []; for (const member of result.rows) { const photo = await command({ command: 'image', args: { catalog, key: member.key } }, 'image', abort.signal); const variant = await command({ command: 'variant', args: { catalog, key: member.key } }, 'variant', abort.signal); rows.push({ photo, variantLabel: variant.label, position: member.cursor.position, provenance: member.provenance_json }); }
      if (!abort.signal.aborted) setPage({ ...result, rows });
    })().catch(e => { if (!abort.signal.aborted) setError(errorText(e)); }).finally(() => { if (!abort.signal.aborted) setBusy(false); }); return () => abort.abort();
  }, [catalog, collection.id, after, refresh, epoch]);
  return <section><h3>Members</h3>{error && <ErrorNotice message={error} />}{busy ? <p role="status">Loading members…</p> : <p>{page.rows.length} members on this page · {page.scanned} candidates checked</p>}
    {!busy && !page.rows.length && <p>{page.next ? 'More candidates remain. Continue to the next page.' : 'End of collection.'}</p>}
    <ul className="organization-list">{page.rows.map(({ photo, variantLabel, position, provenance }) => <li key={photo.image_id}><div><button onClick={() => onChoose(photo)}>{photo.filename} · {variantName(variantLabel)}</button><span>Position {position}</span><Evidence value={provenance} /></div></li>)}</ul>
    <Pager busy={busy} next={page.next} first={() => { setAfter(null); setRefresh(v => v + 1); }} advance={() => setAfter(page.next)} />
  </section>;
}
function BatchHistory({ catalog, onChoose, epoch }: { catalog: string; onChoose: (job: Job) => void; epoch: number }) {
  const [after, setAfter] = useState(''); const [refresh, setRefresh] = useState(0);
  const [names, setNames] = useState<Record<string, Collection | null>>({}); const [page, setPage] = useState<Page<Job, string>>({ rows: [], next: null }); const [error, setError] = useState(''); const [busy, setBusy] = useState(false);
  useEffect(() => { const abort = new AbortController(); setBusy(true); setError(''); setPage({ rows: [], next: null });
    void (async () => { const result = await organize(catalog, { command: 'jobs', args: { after, limit: 20 } }, 'jobs', abort.signal); const names: Record<string, Collection | null> = {}; for (const id of new Set(result.rows.map(jobCollectionId).filter((id): id is string => id !== null))) names[id] = await organize(catalog, { command: 'collection', args: { id } }, 'collection', abort.signal); if (!abort.signal.aborted) { setPage(result); setNames(names); } })().catch(e => { if (!abort.signal.aborted) setError(errorText(e)); }).finally(() => { if (!abort.signal.aborted) setBusy(false); }); return () => abort.abort();
  }, [catalog, after, refresh, epoch]);
  return <section><h3>Saved batches</h3>{error && <ErrorNotice message={error} />}{busy && <p role="status">Loading batches…</p>}{!busy && !page.rows.length && <p>No batches on this page.</p>}<ul className="organization-list">{page.rows.map(job => <li key={job.id}><button onClick={() => onChoose(job)}>{jobName(job, names[jobCollectionId(job) ?? ''])} · {job.state} · {job.applied} applied / {job.pending} pending</button></li>)}</ul><Pager busy={busy} next={page.next} first={() => { setAfter(''); setRefresh(v => v + 1); }} advance={() => setAfter(page.next!)} /></section>;
}
function BatchItems({ catalog, job, title, mutate, onJob }: { catalog: string; job: Job; title: string; mutate: Mutation; onJob: (job: Job) => void }) {
  const [after, setAfter] = useState('0'); const [refresh, setRefresh] = useState(0); const [page, setPage] = useState<Page<JobItem & { photo: GridImage; variantLabel: string }, string>>({ rows: [], next: null });
  const [review, setReview] = useState<{ item: JobItem; identity: ImageIdentity; photo: GridImage; variantLabel: string } | null>(null);
  const [busy, setBusy] = useState(false); const [error, setError] = useState('');
  useEffect(() => { const abort = new AbortController(); setBusy(true); setError(''); setPage({ rows: [], next: null }); setReview(null);
    void (async () => { const result = await organize(catalog, { command: 'items', args: { job: job.id, after, limit: 40 } }, 'items', abort.signal); const rows = []; for (const item of result.rows) { const photo = await command({ command: 'image', args: { catalog, key: item.key } }, 'image', abort.signal); const variant = await command({ command: 'variant', args: { catalog, key: item.key } }, 'variant', abort.signal); rows.push({ ...item, photo, variantLabel: variant.label }); } if (!abort.signal.aborted) setPage({ ...result, rows }); })().catch(e => { if (!abort.signal.aborted) setError(errorText(e)); }).finally(() => { if (!abort.signal.aborted) setBusy(false); }); return () => abort.abort();
  }, [catalog, job.id, job.state, job.applied, job.failed, job.skipped, after, refresh]);
  const inspect = async (item: JobItem & { variantLabel: string }) => { setBusy(true); setError(''); setReview(null); try {
    const identity = await organize(catalog, { command: 'identity', args: { key: item.key } }, 'identity');
    const photo = await command({ command: 'image', args: { catalog, key: item.key } }, 'image'); setReview({ item, identity, photo, variantLabel: item.variantLabel });
  } catch (e) { setError(errorText(e)); } finally { setBusy(false); } };
  const resolve = (item: JobItem, revision: string | null) => void mutate(async () => { onJob(await organize(catalog, { command: 'review', args: { job: job.id, key: item.key, new_revision: revision } }, 'job')); setReview(null); setRefresh(v => v + 1); });
  return <section><h3>Batch items and conflicts</h3>{error && <ErrorNotice message={error} />}{busy && <p role="status">Loading items…</p>}{!busy && !page.rows.length && <p>No items on this page.</p>}<ul className="organization-list">{page.rows.map(item => <li key={item.sequence}><div><span>{item.photo.filename} · {variantName(item.variantLabel)} · {item.status} · reviewed revision {item.expected_revision}{item.result_revision && ` → ${item.result_revision}`}</span><details><summary>Photo identity</summary><p>{item.image_id} · variant {item.key.variant_id}</p></details>{item.error && <p role="alert">{item.error}</p>}{item.status === 'failed' && job.state === 'paused' && <div className="button-group"><button disabled={busy} onClick={() => void inspect(item)}>Review current photo</button><button disabled={busy} onClick={() => resolve(item, null)}>Skip this photo</button></div>}</div></li>)}</ul>
    <Pager busy={busy} next={page.next} first={() => { setAfter('0'); setRefresh(v => v + 1); }} advance={() => setAfter(page.next!)} />
    {review && <div className="organization-review"><h4>Review {review.photo.filename} · {variantName(review.variantLabel)}</h4><p>Selected revision {review.item.expected_revision}; current revision {review.identity.metadata_revision}. Rating {review.photo.rating ?? 'unknown'}, label {review.photo.label || 'none'}, flag {review.photo.flag}.</p><p>Retry will apply “{title}” to this exact current revision. A later change will require another review.</p><button onClick={() => resolve(review.item, review.identity.metadata_revision)}>Accept this revision and retry</button><button onClick={() => setReview(null)}>Keep paused</button></div>}
  </section>;
}

type Props = { open: boolean; onOpen: () => void; catalog: string; selected: GridImage | null; selectedVariantLabel: string | null; rows: GridImage[]; mutate: Mutation; onClose: () => void; onSelect: (row: GridImage) => Promise<void>; onFilter: (filter: Pick<SearchOptions, 'keyword' | 'keyword_direct' | 'collection'>, name: string) => void };
export function OrganizationPanel({ open, onOpen, catalog, selected, selectedVariantLabel, rows, mutate, onClose, onSelect, onFilter }: Props) {
  const [tab, setTab] = useState<'keywords' | 'collections' | 'batches'>('keywords');
  const [keyword, setKeyword] = useState<Keyword | null>(null); const [destination, setDestination] = useState<Keyword | null>(null);
  const [collection, setCollection] = useState<Collection | null>(null); const [parent, setParent] = useState<Collection | null>(null);
  const [placement, setPlacement] = useState<Placement | null>(null); const [position, setPosition] = useState('0'); const [memberPosition, setMemberPosition] = useState('0');
  const [identity, setIdentity] = useState<ImageIdentity | null>(null);
  const [name, setName] = useState(''); const [keywordKind, setKeywordKind] = useState<KeywordKind>('hierarchical'); const [child, setChild] = useState(true);
  const [collectionName, setCollectionName] = useState(''); const [rename, setRename] = useState('');
  const [epoch, setEpoch] = useState(0); const [identityEpoch, setIdentityEpoch] = useState(0); const [error, setError] = useState(''); const [message, setMessage] = useState('');
  const [busy, setBusy] = useState(false); const admission = useRef(false); const mounted = useRef(true);
  const [kind, setKind] = useState<OrganizationOperation['operation']>('rating'); const [rating, setRating] = useState(0); const [label, setLabel] = useState(''); const [flag, setFlag] = useState<'pick' | 'reject' | 'unflagged'>('pick');
  const [checked, setChecked] = useState<string[]>([]); const [job, setJob] = useState<Job | null>(null); const [running, setRunning] = useState(false); const runner = useRef(new OrganizationRunner()); const runAdmission = useRef(false);
  const targetCollection = useNamedCollection(catalog, jobCollectionId(job), epoch);
  const currentParent = useNamedCollection(catalog, placement?.parent ?? null, epoch);
  const jobTargetReady = !jobCollectionId(job) || !!targetCollection.collection;
  const jobTitle = job ? jobName(job, targetCollection.collection) : '';
  const pageNames = useVariantNames(catalog, rows, open && tab === 'batches' && !running);
  const selectionSignature = JSON.stringify(rows.map(row => [row.image_id, row.metadata_revision]));
  useEffect(() => { setChecked([]); }, [selectionSignature]);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; runner.current.stop(); }; }, []);
  const act: Mutation = async action => {
    if (admission.current) return; admission.current = true; setBusy(true); setError(''); setMessage('');
    try { await mutate(action); if (mounted.current) { setIdentityEpoch(v => v + 1); setMessage('Saved to catalog.'); } }
    catch (e) { if (mounted.current) setError(errorText(e)); }
    finally { admission.current = false; if (mounted.current) setBusy(false); }
  };
  const photoKey = selected ? JSON.stringify(selected.key) : '';
  useEffect(() => { const abort = new AbortController(); setIdentity(null); if (selected) void organize(catalog, { command: 'identity', args: { key: selected.key } }, 'identity', abort.signal).then(setIdentity).catch(e => { if (!abort.signal.aborted) setError(errorText(e)); }); return () => abort.abort(); }, [catalog, photoKey, selected?.metadata_revision, identityEpoch]);
  useEffect(() => { const abort = new AbortController(); setPlacement(null); setParent(null); if (collection) void organize(catalog, { command: 'placement', args: { collection: collection.id } }, 'placement', abort.signal).then(value => { setPlacement(value); setPosition(value.position); }).catch(e => { if (!abort.signal.aborted) setError(errorText(e)); }); return () => abort.abort(); }, [catalog, collection?.id, collection?.revision]);
  const chooseCollection = (row: Collection) => { setCollection(row); setRename(row.name); };
  const invalidateCollection = () => { setCollection(null); setPlacement(null); setEpoch(v => v + 1); };
  const operation = (): OrganizationOperation => {
    switch (kind) {
      case 'rating': return { operation: kind, value: rating };
      case 'label': return { operation: kind, value: label };
      case 'flag': return { operation: kind, value: flag };
      case 'add_keyword': case 'remove_keyword': if (!keyword) throw new Error('Choose a keyword first.'); return { operation: kind, kind: keyword.kind, path: keyword.path };
      case 'move_keyword': if (keyword?.kind !== 'hierarchical' || destination?.kind !== 'hierarchical') throw new Error('Choose both hierarchical keywords first.'); return { operation: kind, from: keyword.path, to: destination.path };
      case 'add_collection': case 'remove_collection': if (!collection) throw new Error('Choose a collection first.'); return { operation: kind, collection: collection.id };
    }
  };
  const apply = (op: OrganizationOperation) => { if (!selected) throw new Error('Select a photo first.'); return organize(catalog, { command: 'apply', args: { key: selected.key, expected_revision: selected.metadata_revision, operation: op } }, 'changed'); };
  const append = (jobId: string, chosen: GridImage[]) => organize(catalog, { command: 'append', args: { job: jobId, items: selectionPage(chosen) } }, 'job');
  const chosenRows = rows.filter(row => checked.includes(row.image_id));
  const begin = (chosen: GridImage[]) => void act(async () => { const items = selectionPage(chosen); const created = await organize(catalog, { command: 'begin', args: { operation: operation() } }, 'job'); setJob(created); setEpoch(v => v + 1); setJob(await organize(catalog, { command: 'append', args: { job: created.id, items } }, 'job')); });
  const run = async () => {
    if (!job || !jobTargetReady || runAdmission.current || admission.current) return; runAdmission.current = true; setRunning(true); setError('');
    try { await runner.current.run(job, async id => { let next!: Job; await mutate(async () => { if (!mounted.current) throw new Error('Batch stopped because the catalog is no longer open.'); next = await organize(catalog, { command: 'step', args: { job: id } }, 'job'); }); return next; }, value => { if (mounted.current) setJob(value); }); }
    catch (e) { if (mounted.current) setError(errorText(e)); }
    finally { runAdmission.current = false; if (mounted.current) { setRunning(false); setIdentityEpoch(v => v + 1); setEpoch(v => v + 1); } }
  };
  const cancel = () => { runner.current.stop(); void act(async () => { if (!job) return; setJob(await organize(catalog, { command: 'cancel', args: { job: job.id } }, 'job')); setEpoch(v => v + 1); }); };
  if (!open) return job ? <div className="activity organization-background" role="status"><span>Batch: {jobTitle} · {job.state} · {job.applied} applied · {job.pending} pending{running ? ' · Running' : ''}</span><button onClick={() => { setTab('batches'); onOpen(); }}>Open batch</button>{running && <button onClick={() => runner.current.stop()}>Pause after current photo</button>}{!['complete','cancelled'].includes(job.state) && <button disabled={busy} onClick={cancel}>Cancel remaining items</button>}{error && <ErrorNotice message={error} />}</div> : null;
  return <Dialog title="Organize photographs" onClose={onClose}><div className="organization-panel">
    <nav className="button-group" aria-label="Organization sections">{(['keywords', 'collections', 'batches'] as const).map(value => <button key={value} aria-pressed={tab === value} onClick={() => setTab(value)}>{value === 'batches' ? 'Batch changes' : value === 'keywords' ? 'Keywords' : 'Collections'}</button>)}</nav>
    <p className="hint">{selected ? `Selected photo: ${selected.filename} · ${variantName(selectedVariantLabel)}` : 'Select a photograph in Library to make an individual change.'} · {rows.length} photos on the current Library page.</p>
    {error && <ErrorNotice message={error} dismiss={() => setError('')} />}{message && <p role="status">{message}</p>}{busy && <p role="status">Saving change…</p>}
    <fieldset disabled={busy || running} className="organization-fields">
    {tab === 'keywords' && <div className="organization-columns"><section><h3>Browse keywords</h3><KeywordBrowser catalog={catalog} chosen={keyword} onChoose={setKeyword} epoch={epoch} />
      <form onSubmit={e => { e.preventDefault(); void act(async () => { const path = keywordKind === 'hierarchical' && child && keyword?.kind === 'hierarchical' ? [...keyword.path, name.trim()] : [name.trim()]; await organize(catalog, { command: 'create_keyword', args: { kind: keywordKind, path } }, 'keyword_created'); setName(''); setEpoch(v => v + 1); }); }}><h3>Create keyword</h3><label className="form-field">Name<input value={name} maxLength={1024} onChange={e => setName(e.target.value)} /></label><label className="form-field">Type<select value={keywordKind} onChange={e => setKeywordKind(e.target.value as KeywordKind)}><option value="hierarchical">Hierarchical</option><option value="flat">Flat</option></select></label>{keywordKind === 'hierarchical' && <label className="checkbox"><input type="checkbox" checked={child} onChange={e => setChild(e.target.checked)} disabled={keyword?.kind !== 'hierarchical'} />Create inside {keyword?.kind === 'hierarchical' ? keyword.path.join(' / ') : 'selected keyword (choose one first)'}</label>}<p className="hint">{keywordKind === 'flat' || !child || keyword?.kind !== 'hierarchical' ? 'Creates a root keyword.' : 'Creates a child of the selected keyword.'}</p><button disabled={!name.trim()}>Create keyword</button></form>
    </section><section><h3>{keyword ? keyword.path.join(' / ') : 'Choose a keyword'}</h3>{keyword && <><div className="button-group"><button onClick={() => onFilter({ keyword: keyword.id, keyword_direct: false, collection: null }, keyword.path.join(' / '))}>Browse matching photos</button><button onClick={() => onFilter({ keyword: keyword.id, keyword_direct: true, collection: null }, `${keyword.path.join(' / ')} (direct)`)}>Direct matches only</button></div><div className="button-group"><button disabled={!selected} onClick={() => void act(async () => { await apply({ operation: 'add_keyword', kind: keyword.kind, path: keyword.path }); })}>Add to selected photo</button><button disabled={!selected} onClick={() => void act(async () => { await apply({ operation: 'remove_keyword', kind: keyword.kind, path: keyword.path }); })}>Remove from selected photo</button></div>
      {keyword.kind === 'hierarchical' && <button onClick={() => setDestination(keyword)}>Use as move destination</button>}{destination && <p>Move destination: {destination.path.join(' / ')}</p>}<button disabled={!selected || keyword.kind !== 'hierarchical' || !destination} onClick={() => void act(async () => { if (destination) await apply({ operation: 'move_keyword', from: keyword.path, to: destination.path }); })}>Move selected photo’s keyword to destination</button>
      <details><summary>Delete unused keyword</summary><p>Deletion requires an unused keyword with no children. Remove associations explicitly first.</p><button onClick={() => void act(async () => { await organize(catalog, { command: 'delete_keyword', args: { id: keyword.id } }, 'acknowledged'); if (destination?.id === keyword.id) setDestination(null); setKeyword(null); setEpoch(v => v + 1); })}>Delete {keyword.name}</button></details><Synonyms key={keyword.id} catalog={catalog} keyword={keyword} mutate={act} epoch={epoch} /></>}
    </section></div>}
    {tab === 'collections' && <div className="organization-columns"><section><h3>Browse collections</h3><CollectionBrowser catalog={catalog} chosen={collection} onChoose={chooseCollection} epoch={epoch} />
      <form onSubmit={e => { e.preventDefault(); void act(async () => { await organize(catalog, { command: 'create_collection', args: { name: collectionName.trim() } }, 'collection_created'); setCollectionName(''); setEpoch(v => v + 1); }); }}><label className="form-field">New collection name<input value={collectionName} maxLength={1024} onChange={e => setCollectionName(e.target.value)} /></label><button disabled={!collectionName.trim()}>Create collection</button></form>
      {collection && <><h3>{collection.name}</h3><p>Reviewed collection revision {collection.revision}. Re-select its name from a refreshed page after a conflicting change.</p><Evidence value={collection.provenance_json} /><button onClick={() => onFilter({ collection: collection.id, keyword: null }, collection.name)}>Browse collection in Library</button><form onSubmit={e => { e.preventDefault(); void act(async () => { await organize(catalog, { command: 'rename_collection', args: { id: collection.id, expected_revision: collection.revision, name: rename.trim() } }, 'acknowledged'); invalidateCollection(); }); }}><label className="form-field">Collection name<input value={rename} maxLength={1024} onChange={e => setRename(e.target.value)} /></label><button disabled={!rename.trim() || rename === collection.name}>Rename collection</button></form>
      <details><summary>Delete collection</summary><p>Remove this collection and its effective memberships. Photographs remain in the catalog.</p><button onClick={() => void act(async () => { await organize(catalog, { command: 'delete_collection', args: { id: collection.id, expected_revision: collection.revision } }, 'acknowledged'); invalidateCollection(); })}>Delete {collection.name}</button></details></>}
    </section><section>{collection ? <><h3>Collection placement</h3>{placement && <p>Current position {placement.position}. {placement.parent ? `Current parent: ${currentParent.collection?.name ?? (currentParent.collection === null ? 'deleted or unavailable' : 'loading…')}.` : 'Currently at root.'}</p>}
      {currentParent.error && <ErrorNotice message={currentParent.error} />}<details><summary>Choose parent: {parent?.name ?? 'Root'}</summary><button onClick={() => setParent(null)}>Use root</button><CollectionBrowser catalog={catalog} chosen={parent} onChoose={setParent} epoch={epoch} /></details><label className="form-field">Position<input inputMode="numeric" value={position} maxLength={19} onChange={e => setPosition(e.target.value)} /></label><button disabled={!placement || parent?.id === collection.id} onClick={() => void act(async () => { await organize(catalog, { command: 'place_collection', args: { collection: collection.id, expected_revision: collection.revision, parent: parent?.id ?? null, position: nonnegativeDecimal(position) } }, 'acknowledged'); invalidateCollection(); })}>Place under {parent?.name ?? 'Root'} at this position</button>
      <h3>Selected photo membership</h3>{identity && <p>Reviewed photo metadata revision {identity.metadata_revision}. Original/source identity is checked when saving.</p>}<label className="form-field">Member position<input inputMode="numeric" value={memberPosition} maxLength={19} onChange={e => setMemberPosition(e.target.value)} /></label><div className="button-group"><button disabled={!identity || !selected} onClick={() => void act(async () => { if (!identity) return; await organize(catalog, { command: 'set_member', args: { identity, collection: collection.id, position: nonnegativeDecimal(memberPosition) } }, 'changed'); setEpoch(v => v + 1); })}>Add / reorder selected photo</button><button disabled={!selected} onClick={() => void act(async () => { await apply({ operation: 'remove_collection', collection: collection.id }); setEpoch(v => v + 1); })}>Remove selected photo</button></div>
      <Members key={`${collection.id}:${epoch}`} catalog={catalog} collection={collection} epoch={epoch} onChoose={row => { void onSelect(row).catch(e => setError(errorText(e))); }} />
    </> : <p>Choose a collection to manage placement and members.</p>}</section></div>}
    {tab === 'batches' && <div className="organization-columns"><section><h3>Choose a change</h3><label className="form-field">Operation<select value={kind} onChange={e => setKind(e.target.value as OrganizationOperation['operation'])}>{[['rating','Rating'],['label','Color label'],['flag','Flag'],['add_keyword','Add keyword'],['remove_keyword','Remove keyword'],['move_keyword','Move hierarchical keyword'],['add_collection','Add to collection'],['remove_collection','Remove from collection']].map(([value,text]) => <option key={value} value={value}>{text}</option>)}</select></label>
      {kind === 'rating' && <label className="form-field">Stars<select value={rating} onChange={e => setRating(Number(e.target.value))}>{[0,1,2,3,4,5].map(value => <option key={value} value={value}>{value}</option>)}</select></label>}
      {kind === 'label' && <label className="form-field">Color label (empty clears)<input maxLength={256} value={label} onChange={e => setLabel(e.target.value)} /></label>}{kind === 'flag' && <label className="form-field">Flag<select value={flag} onChange={e => setFlag(e.target.value as typeof flag)}><option value="pick">Pick</option><option value="reject">Reject</option><option value="unflagged">Unflagged</option></select></label>}
      {kind.includes('keyword') && <p>Keyword: {keyword?.path.join(' / ') ?? 'Choose one in Keywords'}.{kind === 'move_keyword' && ` Destination: ${destination?.path.join(' / ') ?? 'Choose in Keywords using “Use as move destination”'}.`} <button onClick={() => setTab('keywords')}>Choose keywords</button></p>}{kind.includes('collection') && <p>Collection: {collection?.name ?? 'Choose one in Collections'}. <button onClick={() => setTab('collections')}>Choose collection</button></p>}
      <h3>Review this Library page</h3>{pageNames.error && <ErrorNotice message={pageNames.error} />}{pageNames.loading && <p role="status">Loading variant names for this page…</p>}<p>{checked.length} of {rows.length} photos checked. This selection covers only the visible Library page, including each chosen variant.</p><div className="button-group"><button onClick={() => setChecked(rows.map(row => row.image_id))}>Check this page only</button><button onClick={() => setChecked([])}>Clear checks</button></div><ul className="organization-list organization-selection">{rows.map(row => <li key={row.image_id}><label className="checkbox"><input type="checkbox" checked={checked.includes(row.image_id)} onChange={e => setChecked(values => e.target.checked ? [...values, row.image_id] : values.filter(id => id !== row.image_id))} />{row.filename} · {variantName(pageNames.names[row.image_id])} · revision {row.metadata_revision}</label></li>)}</ul>
      <div className="button-group"><button disabled={!selected} onClick={() => begin(selected ? [selected] : [])}>Prepare selected photo</button><button disabled={!chosenRows.length || pageNames.loading || !!pageNames.error} onClick={() => begin(chosenRows)}>Prepare {checked.length} checked photos</button></div><p className="hint">Preparing captures reviewed revisions. Append other pages by reopening this saved batch after changing the Library page, then seal and run.</p>
      <BatchHistory catalog={catalog} epoch={epoch} onChoose={value => setJob(value)} />
    </section><section>{job ? <><h3>{jobTitle}</h3>{targetCollection.error && <ErrorNotice message={targetCollection.error} />}{jobCollectionId(job) && targetCollection.collection === null && <p role="alert">This target collection no longer exists. Cancel this batch and prepare a new batch with another collection.</p>}{(job.operation.operation === 'add_collection' || job.operation.operation === 'remove_collection') && <details><summary>Target collection identity</summary><p>{job.operation.collection}</p></details>}<p role="status">{job.state} · {job.pending} pending · {job.applied} applied · {job.failed} failed · {job.skipped} skipped</p>
      {job.state === 'preparing' && <div className="button-group"><button disabled={!selected} onClick={() => void act(async () => { if (selected) setJob(await append(job.id, [selected])); })}>Append selected photo</button><button disabled={!chosenRows.length || pageNames.loading || !!pageNames.error} onClick={() => void act(async () => { setJob(await append(job.id, chosenRows)); })}>Append {checked.length} checked photos</button><button disabled={job.pending === '0'} onClick={() => void act(async () => { setJob(await organize(catalog, { command: 'seal', args: { job: job.id } }, 'job')); })}>Seal reviewed selection</button></div>}
      {['ready','running'].includes(job.state) && <button disabled={!jobTargetReady} onClick={() => void run()}>Run / resume batch</button>}
      <button onClick={() => void act(async () => { setJob(await organize(catalog, { command: 'job', args: { job: job.id } }, 'job')); })}>Refresh saved progress</button>
      {running ? <p>Item details refresh when this run pauses or finishes.</p> : <BatchItems key={job.id} catalog={catalog} job={job} title={jobTitle} mutate={act} onJob={setJob} />}
    </> : <p>Prepare a batch or reopen one from Saved batches. No photos change until the selection is sealed and run.</p>}</section></div>}
    {keyword && collection && <div className="button-group"><button onClick={() => onFilter({ keyword: keyword.id, keyword_direct: false, collection: collection.id }, `${keyword.path.join(' / ')} in ${collection.name}`)}>Browse {keyword.name} within {collection.name}</button><button onClick={() => onFilter({ keyword: keyword.id, keyword_direct: true, collection: collection.id }, `${keyword.path.join(' / ')} (direct) in ${collection.name}`)}>Direct keyword matches within this collection</button></div>}
    </fieldset>
    {job && <div className="organization-run-controls">{running && <><p role="status">Applying one photo at a time… This batch continues while the panel is closed.</p><button onClick={() => runner.current.stop()}>Pause after current photo</button></>}{!['complete','cancelled'].includes(job.state) && <button disabled={busy} onClick={cancel}>Cancel remaining batch items</button>}</div>}
  </div></Dialog>;
}

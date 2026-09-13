import { useEffect, useState, type ReactNode } from 'react';
import { errorText, type VariantKey } from '../bridge';
import { chunkHex, metadata, readableValue, type MetadataChunk, type MetadataData, type MetadataPage, type MetadataRequest, type RetainedText, type RecordRole, type ImportedPage, type ImportedRow, type HistoryDirection } from '../metadata';
import type { ImageIdentity } from '../organization';
import { Dialog, ErrorNotice } from './Controls';
import { SettingsPath } from './SettingsPath';
import { ImportedColumns } from './ImportedColumns';
import './metadata.css';

type ChunkRequest = Extract<MetadataRequest, { command: 'text_chunk' | 'blob_chunk' | 'import_chunk' }>;
type Inspect = (title: string, request: ChunkRequest) => void;
type PageKind = { [K in keyof MetadataData]: MetadataData[K] extends MetadataPage<unknown> ? K : never }[keyof MetadataData];
type PageRow<K extends PageKind> = MetadataData[K] extends MetadataPage<infer T> ? T : never;
type Mutation = (action: () => Promise<void>) => Promise<void>;

function Pager({ busy, next, onFirst, onNext }: { busy: boolean; next: string | null; onFirst: () => void; onNext: () => void }) {
  return <div className="button-group"><button disabled={busy} onClick={onFirst}>First page / Refresh</button><button disabled={busy || next === null} onClick={onNext}>Next page</button></div>;
}

/** The parent keys this component by the query identity; only one bounded page is retained. */
function Pages<K extends PageKind>({ catalog, kind, request, children, first = null }: {
  catalog: string; kind: K; request: (after: string | null) => MetadataRequest;
  children: (row: PageRow<K>, index: number) => ReactNode; first?: string | null;
}) {
  const [after, setAfter] = useState<string | null>(first); const [epoch, setEpoch] = useState(0);
  const [result, setResult] = useState<MetadataPage<PageRow<K>> | null>(null); const [error, setError] = useState(''); const [busy, setBusy] = useState(false);
  const encoded = JSON.stringify(request(after));
  useEffect(() => {
    const abort = new AbortController(); setBusy(true); setError(''); setResult(null);
    void metadata(catalog, JSON.parse(encoded) as MetadataRequest, kind, abort.signal).then(value => { if (!abort.signal.aborted) setResult(value as MetadataPage<PageRow<K>>); }).catch(e => { if (!abort.signal.aborted) setError(errorText(e)); }).finally(() => { if (!abort.signal.aborted) setBusy(false); });
    return () => abort.abort();
  }, [catalog, encoded, kind, epoch]);
  return <div>{error && <ErrorNotice message={error} />}{busy && <p role="status">Loading retained metadata…</p>}
    {result && <>{result.rows.length ? <div className="metadata-rows">{result.rows.map((row, index) => <article key={index}>{children(row, index)}</article>)}</div> : <p>{result.next === null ? 'No entries on this page.' : 'No matching entries in this batch. Continue to the next page.'}</p>}<p className="hint">{result.scanned} entries examined in this batch.</p></>}
    <Pager busy={busy} next={result?.next ?? null} onFirst={() => { setAfter(first); setEpoch(v => v + 1); }} onNext={() => setAfter(result!.next)} />
  </div>;
}

function Value({ label, value, identity, inspect }: { label: string; value: RetainedText | null; identity: ImageIdentity; inspect: Inspect }) {
  if (!value) return <div className="metadata-value"><strong>{label}</strong><span>No effective value</span></div>;
  return <div className="metadata-value"><strong>{label}</strong>{value.inline !== null ? <pre>{readableValue(value.inline)}</pre> : <span>{value.bytes} retained bytes</span>}
    <button onClick={() => inspect(label, { command: 'text_chunk', args: { identity, reference: value.reference, offset: '0', length: 4096 } })}>Inspect exact bytes</button></div>;
}

function ByteViewer({ catalog, title, request, onClose }: { catalog: string; title: string; request: ChunkRequest; onClose: () => void }) {
  const [offset, setOffset] = useState('0'); const [epoch, setEpoch] = useState(0); const [chunk, setChunk] = useState<MetadataChunk | null>(null);
  const [error, setError] = useState(''); const [busy, setBusy] = useState(false); const [format, setFormat] = useState('hex');
  const encoded = JSON.stringify({ ...request, args: { ...request.args, offset } });
  useEffect(() => { const abort = new AbortController(); setBusy(true); setError(''); setChunk(null);
    void metadata(catalog, JSON.parse(encoded) as ChunkRequest, 'chunk', abort.signal).then(value => { if (!abort.signal.aborted) setChunk(value); }).catch(e => { if (!abort.signal.aborted) setError(errorText(e)); }).finally(() => { if (!abort.signal.aborted) setBusy(false); }); return () => abort.abort();
  }, [catalog, encoded, epoch]);
  let content = ''; let decodingError = '';
  if (chunk) { if (format === 'hex') content = chunkHex(chunk.bytes, chunk.offset); else { try { content = new TextDecoder(format, { fatal: true }).decode(Uint8Array.from(chunk.bytes)); } catch { decodingError = 'This byte range is not complete text in the selected encoding. Inspect hexadecimal bytes or choose another encoding.'; } } }
  return <section className="metadata-byte-viewer" aria-label="Retained byte inspector"><header><h3>{title}</h3><button onClick={onClose}>Close byte inspector</button></header>
    <label className="form-field">Display encoding<select value={format} onChange={e => setFormat(e.target.value)}><option value="hex">Hexadecimal / ASCII</option><option value="utf-8">UTF-8 fragment</option><option value="utf-16le">UTF-16 little endian fragment</option><option value="utf-16be">UTF-16 big endian fragment</option></select></label>
    {format !== 'hex' && <p className="hint">Text display covers this byte range only. A character can cross a page boundary; hexadecimal preserves every byte.</p>}
    {error && <ErrorNotice message={error} />}{decodingError && <ErrorNotice message={decodingError} />}{busy && <p role="status">Reading retained bytes…</p>}
    {chunk && <><p>Byte offset {chunk.offset}; {chunk.bytes.length} displayed of {chunk.total} bytes.</p>{chunk.blake3 && <p className="hint">BLAKE3 {chunk.blake3} · {chunk.verified ? 'Complete retained blob verified' : 'Digest not verified'} · {chunk.inspected_bytes} bytes inspected.</p>}<pre tabIndex={0}>{content}</pre></>}
    <Pager busy={busy} next={chunk?.next ?? null} onFirst={() => { setOffset('0'); setEpoch(v => v + 1); }} onNext={() => setOffset(chunk!.next!)} />
  </section>;
}

function Candidates({ catalog, identity, field, inspect, mutate, changed }: { catalog: string; identity: ImageIdentity; field: string; inspect: Inspect; mutate: Mutation; changed: () => void }) {
  const [error, setError] = useState(''); const [busy, setBusy] = useState(false);
  return <section><h3>Candidate values for {field}</h3><p>Choose the effective catalog value. Retained source packets and earlier decisions remain available.</p>{error && <ErrorNotice message={error} />}
    <Pages catalog={catalog} kind="candidates" request={after => ({ command: 'candidates', args: { identity, field, after, limit: 20 } })}>{row => <>
      <Value label="Candidate value" value={row.value} identity={identity} inspect={inspect} />{row.ambiguous && <p className="hint">This candidate has an ambiguous source association.</p>}
      <details><summary>Source reference</summary><p>Source {row.source} · observation {row.observation} · model {row.model}</p><code>{row.semantic_hash}</code></details>
      <button disabled={busy} onClick={() => { setBusy(true); setError(''); void mutate(async () => { await metadata(catalog, { command: 'resolve', args: { key: identity.key, expected_revision: identity.metadata_revision, field, model: row.model } }, 'changed'); changed(); }).catch(e => setError(errorText(e))).finally(() => setBusy(false)); }}>Use this value</button>
    </>}</Pages>
  </section>;
}

function Observations({ catalog, identity, inspect }: { catalog: string; identity: ImageIdentity; inspect: Inspect }) {
  const [selected, setSelected] = useState<string | null>(null); const [view, setView] = useState<'models' | 'packets'>('packets');
  return <><Pages catalog={catalog} kind="observations" request={after => ({ command: 'observations', args: { identity, after, limit: 20 } })}>{row => <>
    <p>{row.current ? 'Current observation' : 'Retained earlier observation'} · source {row.source}</p>{(['revision', 'status', 'issues', 'provenance', 'created_at'] as const).map(name => <Value key={name} label={name.replaceAll('_', ' ')} value={row[name]} identity={identity} inspect={inspect} />)}
    <button aria-pressed={selected === row.id} onClick={() => setSelected(row.id)}>Inspect packets and models</button>
  </>}</Pages>{selected && <section><h3>Retained observation {selected}</h3><nav aria-label="Observation data"><button aria-pressed={view === 'packets'} onClick={() => setView('packets')}>Original packets</button><button aria-pressed={view === 'models'} onClick={() => setView('models')}>Parsed models</button></nav>
    {view === 'packets' ? <Pages key={`packets:${selected}`} catalog={catalog} kind="packets" request={after => ({ command: 'packets', args: { identity, observation: selected, after, limit: 20 } })}>{row => <><Value label="Packet descriptor" value={row.descriptor} identity={identity} inspect={inspect} /><p>{row.bytes} bytes</p><button onClick={() => inspect('Original packet', { command: 'blob_chunk', args: { key: identity.key, source: { kind: 'packet', observation: selected, ordinal: row.ordinal }, offset: '0', length: 4096 } })}>Inspect retained packet bytes</button></>}</Pages>
      : <Pages key={`models:${selected}`} catalog={catalog} kind="models" request={after => ({ command: 'models', args: { identity, observation: selected, after, limit: 20 } })}>{row => <>{(['descriptor', 'projection', 'error'] as const).map(name => <Value key={name} label={name} value={row[name]} identity={identity} inspect={inspect} />)}<button onClick={() => inspect('Retained model', { command: 'blob_chunk', args: { key: identity.key, source: { kind: 'model', id: row.id }, offset: '0', length: 4096 } })}>Inspect retained model bytes</button></>}</Pages>}
  </section>}</>;
}

function ImportedProvenance({ row, title }: { row: ImportedRow; title: string }) {
  return <details><summary>{title}</summary><dl><dt>Source row identity</dt><dd>{row.source_id}</dd><dt>Retained record</dt><dd>{row.record}</dd><dt>Table descriptor</dt><dd>{row.table_record}</dd><dt>Entity descriptor</dt><dd>{row.entity_record}</dd><dt>Classification</dt><dd>{row.classification}</dd><dt>Retained cells field</dt><dd>{row.cells_field} · {row.cells_json_bytes} bytes</dd></dl><p>Exact source key (capture revision, table, and key)</p><pre>{row.source_key_json}</pre></details>;
}

function ImportedHistory({ catalog, variant, inspect }: { catalog: string; variant: VariantKey; inspect: Inspect }) {
  const [anchor, setAnchor] = useState<string | null>(null); const [after, setAfter] = useState<string | null>(null); const [direction, setDirection] = useState<HistoryDirection>('Outgoing');
  const [page, setPage] = useState<ImportedPage | null>(null); const [error, setError] = useState(''); const [busy, setBusy] = useState(false); const [refresh, setRefresh] = useState(0);
  const [settingsOpen, setSettingsOpen] = useState(false); const [role, setRole] = useState<RecordRole>('row'); const [column, setColumn] = useState(''); const [settingsPath, setSettingsPath] = useState('[]'); const [interpret, setInterpret] = useState<{ column: string; path: string } | null>(null);
  const query = JSON.stringify({ key: variant, anchor_json: anchor, direction, after_json: after, limit: 1 });
  useEffect(() => { const abort = new AbortController(); setPage(null); setError(''); setBusy(true); setInterpret(null); setColumn(''); setSettingsPath('[]'); setSettingsOpen(false);
    void metadata(catalog, { command: 'import_history', args: JSON.parse(query) }, 'import_history', abort.signal).then(value => { if (!abort.signal.aborted) setPage(value); }).catch(e => { if (!abort.signal.aborted) setError(errorText(e)); }).finally(() => { if (!abort.signal.aborted) setBusy(false); }); return () => abort.abort();
  }, [catalog, query, refresh]);
  return <section><p>Browse the imported Lightroom record, its linked history, and retained settings. Retention does not mean Adobe rendering equivalence.</p>
    <label className="form-field">Relation direction<select value={direction} onChange={e => { setDirection(e.target.value as HistoryDirection); setAfter(null); }}><option value="Outgoing">Referenced records</option><option value="Incoming">Records referring here</option></select></label>
    <button onClick={() => { setAnchor(null); setAfter(null); setRefresh(v => v + 1); }}>Selected photo record</button>{error && <ErrorNotice message={error} />}{busy && <p role="status">Loading imported history…</p>}
    {page && <><p>Record classification: {page.row.classification}</p><p className="hint">Retained relationship/index coverage {page.coverage_complete ? 'complete' : 'incomplete'} · keys {page.keys_complete ? 'complete' : 'incomplete'} · Adobe rendering equivalence: {page.adobe_rendering_equivalent ? 'reported' : 'not established'}.</p>
      <details><summary>Retained input identity</summary><pre>{page.input}</pre></details><ImportedProvenance row={page.row} title="Current record provenance" />
      {page.relations.map(row => <article key={row.reference_record}><p>{row.field} → {row.target_table} · {row.compatibility}</p><details><summary>Relationship provenance</summary><p>Reference {row.reference_record} · source {row.source_id}</p><p>Exact target key</p><pre>{row.target_key}</pre></details>{row.source && <ImportedProvenance row={row.source} title="Source record provenance" />}{row.target && <ImportedProvenance row={row.target} title="Target record provenance" />}<button disabled={!row.anchor_json} onClick={() => { setAnchor(row.anchor_json); setAfter(null); }}>Open linked record</button></article>)}
      <Pager busy={busy} next={page.next} onFirst={() => { setAfter(null); setRefresh(v => v + 1); }} onNext={() => { setAnchor(page.anchor_json); setAfter(page.next); }} />
      <label className="form-field">Retained record data<select value={role} onChange={e => setRole(e.target.value as RecordRole)}><option value="row">Row fields</option><option value="table">Table descriptor</option><option value="entity">Entity descriptor</option></select></label>
      <Pages key={`${page.anchor_json}:${role}`} catalog={catalog} kind="import_fields" first="" request={cursor => ({ command: 'import_fields', args: { key: variant, anchor_json: page.anchor_json, role, after: cursor ?? '', limit: 20 } })}>{row => <><strong>{row.name}</strong><p>{row.representation} · {row.bytes} bytes</p>{row.scalar_json !== null && <pre>{row.scalar_json}</pre>}<button onClick={() => inspect(row.name, { command: 'import_chunk', args: { key: variant, anchor_json: page.anchor_json, role, field: row.name, offset: '0', length: 4096 } })}>Inspect retained field bytes</button></>}</Pages>
      <details open={settingsOpen} onToggle={e => { const open = e.currentTarget.open; setSettingsOpen(open); if (!open) { setSettingsPath('[]'); setInterpret(null); } }}><summary>Adobe settings interpretation</summary>{settingsOpen && <><p>Choose a retained row column and its settings path. The original field remains accessible even when interpretation is unavailable.</p>
        <ImportedColumns key={page.anchor_json} catalog={catalog} variant={variant} anchor={page.anchor_json} selected={column} choose={setColumn} /><p>Selected column: {column || 'Choose a column above'}</p><SettingsPath key={page.anchor_json} onChange={setSettingsPath} /><button disabled={!column} onClick={() => setInterpret({ column, path: settingsPath })}>Inspect settings</button>
        {interpret && <AdobeSettings key={JSON.stringify([page.anchor_json, interpret])} catalog={catalog} variant={variant} anchor={page.anchor_json} column={interpret.column} path={interpret.path} />}
      </>}</details>
    </>}
  </section>;
}

function AdobeSettings({ catalog, variant, anchor, column, path }: { catalog: string; variant: VariantKey; anchor: string; column: string; path: string }) {
  const [after, setAfter] = useState('0'); const [epoch, setEpoch] = useState(0); const [result, setResult] = useState<MetadataData['adobe_properties'] | null>(null); const [error, setError] = useState(''); const [busy, setBusy] = useState(false);
  const query = JSON.stringify({ key: variant, anchor_json: anchor, column, settings_path_json: path, after, limit: 20 });
  useEffect(() => { const abort = new AbortController(); setResult(null); setError(''); setBusy(true); void metadata(catalog, { command: 'adobe_properties', args: JSON.parse(query) }, 'adobe_properties', abort.signal).then(value => { if (!abort.signal.aborted) setResult(value); }).catch(e => { if (!abort.signal.aborted) setError(errorText(e)); }).finally(() => { if (!abort.signal.aborted) setBusy(false); }); return () => abort.abort(); }, [catalog, query, epoch]);
  return <>{error && <ErrorNotice message={error} />}{busy && <p role="status">Interpreting retained settings…</p>}{result && <><p>{result.compatibility}: {result.reason}</p><p>Coordinate space: {result.coordinate_space ?? 'Not established'}</p>{result.failure !== null && <pre>{JSON.stringify(result.failure)}</pre>}{result.missing.length > 0 && <p>Missing: {result.missing.join(', ')}</p>}{result.properties.rows.map((row, index) => <article key={index}><strong>{row.name}</strong><pre>{row.lexical}</pre><p>{row.disposition}: {row.reason}</p><details><summary>Retained property details</summary><pre>{row.path_json}</pre><pre>{row.value_json}</pre><p>Bytes {row.start}–{row.end}; namespace {row.namespace ?? 'none'}</p></details></article>)}{result.input_json && <details><summary>Retained interpretation source</summary><pre>{result.input_json}</pre></details>}{result.contribution_json && <details><summary>Native edit contribution</summary><pre>{result.contribution_json}</pre></details>}</>}<Pager busy={busy} next={result?.properties.next ?? null} onFirst={() => { setAfter('0'); setEpoch(v => v + 1); }} onNext={() => setAfter(result!.properties.next!)} /></>;
}

export function MetadataPanel({ catalog, variant, filename, mutate, onClose }: { catalog: string; variant: VariantKey; filename: string; mutate: Mutation; onClose: () => void }) {
  const [identity, setIdentity] = useState<ImageIdentity | null>(null); const [epoch, setEpoch] = useState(0); const [error, setError] = useState('');
  const [tab, setTab] = useState('fields'); const [field, setField] = useState<string | null>(null); const [bytes, setBytes] = useState<{ title: string; request: ChunkRequest } | null>(null);
  const key = JSON.stringify(variant);
  useEffect(() => { const abort = new AbortController(); setIdentity(null); setField(null); setBytes(null); setError(''); void metadata(catalog, { command: 'identity', args: { key: variant } }, 'identity', abort.signal).then(value => { if (!abort.signal.aborted) setIdentity(value); }).catch(e => { if (!abort.signal.aborted) setError(errorText(e)); }); return () => abort.abort(); }, [catalog, key, epoch]);
  const inspect: Inspect = (title, request) => setBytes({ title, request });
  const tabs = [['fields', 'Effective values'], ['sources', 'Sources'], ['observations', 'Packets & observations'], ['decisions', 'Decisions'], ['file_instances', 'File provenance'], ['imported', 'Lightroom history']];
  return <Dialog title={`Metadata · ${filename}`} onClose={onClose}><div className="metadata-panel"><nav aria-label="Metadata sections">{tabs.map(([id, title]) => <button key={id} aria-pressed={tab === id} onClick={() => { setTab(id); setBytes(null); }}>{title}</button>)}</nav><button onClick={() => setEpoch(v => v + 1)}>Refresh current metadata</button>
    {error && <ErrorNotice message={error} />}{!identity && !error && <p role="status">Loading selected photo metadata…</p>}
    {identity && <div key={JSON.stringify(identity)}>
      {tab === 'fields' && <><Pages catalog={catalog} kind="fields" request={after => ({ command: 'fields', args: { identity, after, limit: 20 } })}>{row => <><Value label={row.name} value={row.value} identity={identity} inspect={inspect} />{row.conflicted && <p className="metadata-conflict">Conflicting source values</p>}<button onClick={() => setField(row.name)}>Review candidate values</button></>}</Pages>{field && <Candidates key={field} catalog={catalog} identity={identity} field={field} inspect={inspect} mutate={mutate} changed={() => setEpoch(v => v + 1)} />}</>}
      {tab === 'sources' && <Pages catalog={catalog} kind="sources" request={after => ({ command: 'sources', args: { identity, after, limit: 20 } })}>{row => <>{(['display', 'kind', 'association', 'availability', 'locator', 'status', 'issues'] as const).map(name => <Value key={name} label={name} value={row[name]} identity={identity} inspect={inspect} />)}<details><summary>Source reference</summary><p>Source {row.id} · observation {row.observation ?? 'none'}</p></details></>}</Pages>}
      {tab === 'observations' && <Observations catalog={catalog} identity={identity} inspect={inspect} />}
      {tab === 'decisions' && <Pages catalog={catalog} kind="decisions" request={after => ({ command: 'decisions', args: { identity, after, limit: 20 } })}>{row => <><p>Metadata revision {row.revision}</p>{(['action', 'detail', 'created_at'] as const).map(name => <Value key={name} label={name.replaceAll('_', ' ')} value={row[name]} identity={identity} inspect={inspect} />)}</>}</Pages>}
      {tab === 'file_instances' && <Pages catalog={catalog} kind="file_instances" request={after => ({ command: 'file_instances', args: { identity, after, limit: 20 } })}>{row => <><Value label="File provenance" value={row.provenance} identity={identity} inspect={inspect} /><Value label="Observed at" value={row.observed_at} identity={identity} inspect={inspect} /><details><summary>Source reference</summary><p>File instance {row.id} · source {row.source}</p></details></>}</Pages>}
      {tab === 'imported' && <ImportedHistory catalog={catalog} variant={variant} inspect={inspect} />}
    </div>}
    {bytes && <ByteViewer key={JSON.stringify(bytes)} catalog={catalog} title={bytes.title} request={bytes.request} onClose={() => setBytes(null)} />}
  </div></Dialog>;
}

import { useEffect, useState } from 'react';
import { errorText, type VariantKey } from '../bridge';
import { metadata, type ImportedColumns as Columns } from '../metadata';
import { ErrorNotice } from './Controls';

export function ImportedColumns({ catalog, variant, anchor, selected, choose }: { catalog: string; variant: VariantKey; anchor: string; selected: string; choose: (name: string) => void }) {
  const [after, setAfter] = useState('0'); const [page, setPage] = useState<Columns | null>(null); const [error, setError] = useState(''); const [busy, setBusy] = useState(false); const [epoch, setEpoch] = useState(0);
  const query = JSON.stringify({ key: variant, anchor_json: anchor, after, limit: 20 });
  useEffect(() => { const abort = new AbortController(); setBusy(true); setError(''); setPage(null);
    void metadata(catalog, { command: 'import_columns', args: JSON.parse(query) }, 'import_columns', abort.signal).then(value => { if (!abort.signal.aborted) setPage(value); }).catch(e => { if (!abort.signal.aborted) setError(errorText(e)); }).finally(() => { if (!abort.signal.aborted) setBusy(false); }); return () => abort.abort();
  }, [catalog, query, epoch]);
  return <section aria-label="Retained Lightroom columns"><h3>Choose a settings column</h3>{error && <ErrorNotice message={error} />}{busy && <p role="status">Reading retained column names…</p>}
    {page && <><p className="hint">{page.reason}{!page.types_complete && ' Some cell types are unknown; exact retained row bytes remain accessible.'}</p><div className="metadata-columns">{page.columns.rows.map(row => <button key={row.ordinal} aria-pressed={selected === row.name} onClick={() => choose(row.name)}>{row.name} <span className="hint">{row.cell_type ?? 'Unknown type'}{row.bytes !== null ? ` · ${row.bytes} bytes` : ''}</span></button>)}</div>{!page.columns.rows.length && <p>{page.columns.next ? 'Continue to the next column page.' : 'No columns on this page.'}</p>}</>}
    <div className="button-group"><button disabled={busy} onClick={() => { setAfter('0'); setEpoch(v => v + 1); }}>First column page / Refresh</button><button disabled={busy || !page?.columns.next} onClick={() => setAfter(page!.columns.next!)}>Next column page</button></div>
  </section>;
}

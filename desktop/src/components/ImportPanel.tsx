import { useEffect, useRef, useState } from 'react';
import { chooseSource, command, displayPath, errorText, type ImportStatus, type NativePath } from '../bridge';
import { beginMeasurementImportStatus, discardMeasurementImportStatus, setMeasurementImportStatus } from '../performanceMeasurement';
import { Dialog, ErrorNotice } from './Controls';

const active = (status: ImportStatus | null) => status && ['discovering', 'draining', 'cancel_requested'].includes(status.phase);
export function ImportPanel({ catalog, jobsHeld, open, onProgress, onComplete, onClose }: { catalog: string; open: boolean; jobsHeld: boolean; onProgress: (status: ImportStatus | null) => void; onComplete: () => void; onClose: () => void }) {
  const [source, setSource] = useState<{ path: NativePath; display: string } | null>(null);
  const [status, setStatus] = useState<ImportStatus | null>(null);
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const admitted = useRef(false);
  const previous = useRef<string | null>(null);
  const complete = useRef(onComplete); complete.current = onComplete;
  const progress = useRef(onProgress); progress.current = onProgress;
  const current = useRef<ImportStatus | null>(null);
  const observed = (next: ImportStatus | null, event?: number) => {
    setMeasurementImportStatus(next, event); current.current = next; setStatus(next); progress.current(next);
  };
  const requestStatus = async (action: () => Promise<ImportStatus | null>, knownActive: boolean, signal: AbortSignal) => {
    const event = knownActive ? beginMeasurementImportStatus() : undefined;
    try {
      const next = await action();
      if (signal.aborted) { discardMeasurementImportStatus(event); return next; }
      observed(next, event); return next;
    }
    catch (failure) { discardMeasurementImportStatus(event); throw failure; }
  };
  useEffect(() => {
    const abort = new AbortController(); let timer: ReturnType<typeof setTimeout>;
    const poll = async () => {
      try {
        const next = await requestStatus(
          () => command({ command: 'import_status', args: { catalog } }, 'import', abort.signal),
          !!active(current.current),
          abort.signal,
        );
        if (abort.signal.aborted) return;
        const terminal = next && !active(next) ? `${next.id}:${next.phase}` : null;
        if (terminal && terminal !== previous.current) complete.current();
        previous.current = terminal;
      } catch (e) { if (!abort.signal.aborted) setError(errorText(e)); }
      if (!abort.signal.aborted) timer = setTimeout(() => { void poll(); }, 500);
    };
    void poll(); return () => { abort.abort(); clearTimeout(timer); };
  }, [catalog]);
  const run = async (action: () => Promise<void>) => {
    if (admitted.current) return; admitted.current = true; setBusy(true);
    try { await action(); setError(''); } catch (e) { setError(errorText(e)); }
    finally { admitted.current = false; setBusy(false); }
  };
  const start = async (resume: boolean) => {
    if (!source || jobsHeld) return;
    await run(async () => { observed(await command({ command: resume ? 'import_resume' : 'import_start', args: { catalog, source: source.path } }, 'import')); });
  };
  if (!open) return null;
  return <Dialog title="Add photographs" onClose={onClose}>
    <p>Add a folder and its subfolders to this catalog. Originals stay in their existing locations. Every year can share this library.</p>
    {jobsHeld && <p role="status">Import is held until this restored catalog’s pending jobs have been reviewed.</p>}
    {error && <ErrorNotice message={error} />}
    <button disabled={busy || !!active(status)} onClick={() => void run(async () => { const choice = await chooseSource(); if (choice) setSource(choice); })}>Choose source folder…</button>
    {source && <p className="source-path">{source.display}</p>}
    <div className="button-group"><button className="primary" disabled={busy || jobsHeld || !source || !!active(status)} onClick={() => void start(false)}>Add photographs</button><button disabled={busy || jobsHeld || !source || !!active(status)} onClick={() => void start(true)}>Resume folder scan</button></div>
    <p className="hint">Resume scans the chosen folder again and skips unchanged files already cataloged. Counters describe this scan attempt.</p>
    {status && <section aria-label="Import progress"><p role="status">{status.phase.replaceAll('_', ' ')}{status.pending_previews > 0 ? ` · ${status.pending_previews} previews pending` : ''}</p>
      <dl className="import-counts"><dt>Imported</dt><dd>{status.imported}</dd><dt>Unchanged</dt><dd>{status.unchanged}</dd><dt>Failed</dt><dd>{status.failed}</dd><dt>Skipped</dt><dd>{status.skipped}</dd><dt>Metadata updated</dt><dd>{status.metadata_updated}</dd><dt>Metadata warnings</dt><dd>{status.metadata_warnings}</dd><dt>Awaiting resources</dt><dd>{status.awaiting_resources}</dd></dl>
      {status.error_source && <p className="source-path">{displayPath(status.error_source)}</p>}
      {status.error && <ErrorNotice message={status.error} />}
      {active(status) && <button disabled={busy || status.phase === 'cancel_requested'} onClick={() => void run(async () => { observed(await command({ command: 'import_cancel', args: { catalog, import: status.id } }, 'import')); })}>Cancel import</button>}
    </section>}
    <p className="hint">Closing this panel keeps the import running. Close the catalog to stop its background work safely.</p>
  </Dialog>;
}

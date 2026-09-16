import { useEffect, useRef, useState } from 'react';
import { chooseLocation, command, errorText, type BackupStatus, type Data, type NativePath } from '../bridge';
import { Dialog, ErrorNotice } from './Controls';

type Location = { path: NativePath; display: string };
const running = (status: BackupStatus | null) => status && ['running', 'cancel_requested'].includes(status.state);
export function BackupPanel({ catalog, open, onClose, onProgress }: { catalog: string | null; open: boolean; onClose: () => void; onProgress: (status: BackupStatus | null) => void }) {
  const [mode, setMode] = useState<'create' | 'inspect' | 'restore'>('create');
  const [bundle, setBundle] = useState<Location | null>(null);
  const [destination, setDestination] = useState<Location | null>(null);
  const [status, setStatus] = useState<BackupStatus | null>(null);
  const [restore, setRestore] = useState<Data['restore']>(null);
  const [acknowledge, setAcknowledge] = useState(false);
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  type Admission = { previous?: string | null; generation: object };
  const admitted = useRef<Admission | null>(null);
  const scope = useRef({ catalog, open });
  if (scope.current.catalog !== catalog || scope.current.open !== open) scope.current = { catalog, open };
  const latest = useRef<BackupStatus | null>(null);
  const cancelAdmission = useRef<string | null>(null);
  const [canceling, setCanceling] = useState<string | null>(null);
  const [ready, setReady] = useState(false);
  const progress = useRef(onProgress); progress.current = onProgress;
  useEffect(() => {
    const abort = new AbortController(); let timer: ReturnType<typeof setTimeout>;
    const poll = async () => {
      try { const next = await command({ command: 'backup_status' }, 'backup', abort.signal); if (!abort.signal.aborted) {
        latest.current = next; setStatus(next); progress.current(next); setReady(true);
        if (next && admitted.current?.previous !== undefined && next.operation !== admitted.current.previous) {
          admitted.current = null; setBusy(false);
        }
        if (cancelAdmission.current && (next?.operation !== cancelAdmission.current || !running(next) || next.state === 'cancel_requested')) {
          cancelAdmission.current = null; setCanceling(null);
        }
      } }
      catch (e) { if (!abort.signal.aborted) { setReady(false); setError(errorText(e)); } }
      if (!abort.signal.aborted) timer = setTimeout(() => { void poll(); }, 750);
    };
    void poll(); return () => { abort.abort(); clearTimeout(timer); };
  }, []);
  useEffect(() => {
    setMode('create'); setBundle(null); setDestination(null); setError('');
    admitted.current = null; setBusy(false);
  }, [catalog]);
  useEffect(() => {
    const abort = new AbortController(); setRestore(null); setAcknowledge(false);
    // A hidden review has no remaining picker/release UI ownership. Keep only
    // an uncertain global backup start until polling or its reply resolves it.
    if (admitted.current?.previous === undefined) { admitted.current = null; setBusy(false); }
    if (catalog && open) void command({ command: 'restore_status', args: { catalog } }, 'restore', abort.signal).then(next => { if (!abort.signal.aborted) setRestore(next); }).catch(e => { if (!abort.signal.aborted) setError(errorText(e)); });
    return () => abort.abort();
  }, [catalog, open]);
  const run = async (action: (current: () => boolean) => Promise<void>, starting = false) => {
    if (admitted.current) return;
    const ticket: Admission = { generation: scope.current, ...(starting ? { previous: latest.current?.operation ?? null } : {}) };
    admitted.current = ticket; setBusy(true);
    const current = () => scope.current === ticket.generation && admitted.current === ticket;
    let acknowledged = false;
    try { await action(current); acknowledged = true; if (current()) setError(''); }
    catch (e) { if (current()) setError(errorText(e)); }
    finally { if (admitted.current === ticket && (!starting || !acknowledged)) { admitted.current = null; setBusy(false); } }
  };
  const start = async () => run(async () => {
    // Status has a single writer: the polling loop. A delayed acknowledgement
    // cannot replace a newer operation or regress completed work to running.
    if (mode === 'create' && catalog && destination) await command({ command: 'backup_create', args: { catalog, bundle: destination.path } }, 'backup');
    else if (mode === 'inspect' && bundle) await command({ command: 'backup_inspect', args: { bundle: bundle.path } }, 'backup');
    else if (mode === 'restore' && bundle && destination) await command({ command: 'backup_restore', args: { bundle: bundle.path, destination: destination.path } }, 'backup');
  }, true);
  const cancel = (operation: string) => {
    if (cancelAdmission.current === operation || latest.current?.operation !== operation) return;
    cancelAdmission.current = operation; setCanceling(operation);
    void command({ command: 'backup_cancel', args: { operation } }, 'backup').catch(e => {
      if (cancelAdmission.current === operation) { setError(errorText(e)); cancelAdmission.current = null; setCanceling(null); }
    });
  };
  if (!open) return null;
  const blocked = busy || !ready || !!running(status);
  return <Dialog title="Backup and restore" onClose={onClose}>
    <p>Backups contain committed catalog metadata, edits, and retained source data. Originals and preview caches are stored separately.</p>
    {error && <ErrorNotice message={error} />}
    {!ready && <p role="status">Checking backup operation status…</p>}
    {busy && admitted.current?.previous !== undefined && <p role="status">Waiting for backup admission. Operation status is checked automatically.</p>}
    <div className="button-group" aria-label="Backup action">{(['create', 'inspect', 'restore'] as const).map(value => <button key={value} disabled={blocked} aria-pressed={mode === value} onClick={() => { setMode(value); setDestination(null); }}>{value === 'create' ? 'Back up catalog' : value === 'inspect' ? 'Verify backup' : 'Restore backup'}</button>)}</div>
    {mode === 'create' && !catalog && <p>Open a catalog before creating its backup.</p>}
    {mode !== 'create' && <><button disabled={blocked} onClick={() => void run(async current => { const choice = await chooseLocation('backup_bundle'); if (choice && current()) setBundle(choice); })}>Choose backup folder…</button>{bundle && <p className="source-path">{bundle.display}</p>}</>}
    {mode !== 'inspect' && <><button disabled={blocked} onClick={() => void run(async current => { const choice = await chooseLocation(mode === 'create' ? 'new_backup' : 'new_restore'); if (choice && current()) setDestination(choice); })}>Choose new destination…</button>{destination && <p className="source-path">{destination.display}</p>}<p className="hint">The destination must be new. An existing catalog or backup will not be overwritten.</p></>}
    <button className="primary" disabled={blocked || (mode === 'create' ? !catalog || !destination : mode === 'inspect' ? !bundle : !bundle || !destination)} onClick={() => void start()}>{mode === 'create' ? 'Create backup' : mode === 'inspect' ? 'Verify contents' : 'Restore to new catalog'}</button>
    {status && <section aria-label="Backup progress"><p role="status">{status.kind} · {status.state.replaceAll('_', ' ')}{status.progress ? ` · ${status.progress.phase}` : ''}</p>
      {status.progress && <p className="hint">{status.progress.pages_copied} of {status.progress.total_pages} pages · {status.progress.bytes_processed} bytes processed in this phase</p>}
      {status.error && <ErrorNotice message={`${status.error.message}${status.error.truncated ? ' (message truncated)' : ''}`} />}
      {status.receipt && <><p>Completed and verified.</p><p className="hint source-path">{status.receipt.kind === 'backup' ? `Backup ${status.receipt.data.backup_id} · ${status.receipt.data.database_bytes} bytes` : `Restored catalog ${status.receipt.data.restore_id}. Open the new catalog folder to browse it.`}</p></>}
      {running(status) && <button disabled={canceling === status.operation || status.state === 'cancel_requested'} onClick={() => cancel(status.operation)}>Cancel operation</button>}
    </section>}
    {restore && <section aria-label="Restored catalog"><h3>Restored catalog</h3><p className="hint source-path">Receipt {restore.receipt.restore_id}</p><p>{restore.jobs_held ? 'Preexisting external jobs are held. Browsing, editing, relinking and requested previews remain available.' : 'The restored catalog’s jobs have been enabled.'}</p>
      {restore.jobs_held && <><p>Review pending jobs and their destinations before enabling them. They may include writes to original metadata or sidecars, exports, imports and migrations saved in this backup.</p><label className="checkbox"><input type="checkbox" checked={acknowledge} onChange={event => setAcknowledge(event.target.checked)} />I acknowledge the preexisting external jobs in this restored catalog.</label><button disabled={busy || !acknowledge} onClick={() => void run(async current => { if (catalog) { const next = await command({ command: 'resume_restored_jobs', args: { catalog, restore_id: restore.receipt.restore_id, acknowledge_pending_jobs: true } }, 'restore'); if (current()) { setRestore(next); setAcknowledge(false); } } })}>Enable pending external jobs</button></>}
    </section>}
    <p className="hint">Closing this panel keeps the operation running. Closing the catalog or quitting the app cancels and joins its background work.</p>
  </Dialog>;
}

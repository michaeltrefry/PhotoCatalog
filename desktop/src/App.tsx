import { useCallback, useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { command, chooseFolder, desktopAvailable, errorText, imageKey, type BackupStatus, type CatalogStatus, type CullOperation, type Data, type Folder, type GridImage, type HistoryEntry, type ImportStatus, type Variant } from './bridge';
import { Dialog, ErrorNotice, Section } from './components/Controls';
import { FolderTree } from './components/FolderTree';
import { CatalogActivity } from './components/CatalogActivity';
import { OrganizationPanel } from './components/OrganizationPanel';
import { RelinkPanel } from './components/RelinkPanel';
import { useRelink } from './state/useRelink';
import { relinkTerminal } from './relink';
import { CopyPanel } from './components/CopyPanel';
import { useEditCopy } from './state/useEditCopy';
import { copyTerminal } from './editCopy';
import { usePhotoExport } from './state/usePhotoExport';
import { terminal as exportTerminal } from './photoExport';
import { ExportPanel, type ExportGate } from './components/ExportPanel';
import { MetadataPanel } from './components/MetadataPanel';
import { BackupPanel } from './components/BackupPanel';
import { SearchFilters, defaultFilters } from './components/SearchFilters';
import { ImportPanel } from './components/ImportPanel';
import { PhotoGrid, Filmstrip } from './components/PhotoGrid';
import { RecipeControls } from './components/RecipeControls';
import { CompatibilityStatus } from './components/CompatibilityStatus';
import { Viewport } from './components/Viewport';
import { EditQueue, type EditSnapshot } from './state/editQueue';
import { ActionGate } from './state/actionGate';

const initialStatus: CatalogStatus = { phase: 'closed', catalog: null, jobs_held: false, pending_commands: 0, active_previews: 0, cancel_requested: false, message: null };
export function App() {
  const [status, setStatus] = useState(initialStatus);
  const [catalogName, setCatalogName] = useState('');
  const [mode, setMode] = useState<'library' | 'cull' | 'develop'>('library');
  const [scope, setScope] = useState<Folder | null | undefined>();
  const [recursive, setRecursive] = useState(true);
  const [search, setSearch] = useState('');
  const [appliedSearch, setAppliedSearch] = useState('');
  const [filters, setFilters] = useState(defaultFilters);
  const [showFilters, setShowFilters] = useState(false);
  const [page, setPage] = useState<Data['images']>({ rows: [], next: null, has_more: false, page_complete: true, scanned: 0 });
  const [cursor, setCursor] = useState<string | null>(null);
  const [previous, setPrevious] = useState<(string | null)[]>([]);
  const [refresh, setRefresh] = useState(0);
  const [folderEpoch, setFolderEpoch] = useState(0);
  const [showImport, setShowImport] = useState(false);
  const [showBackup, setShowBackup] = useState(false);
  const [showOrganization, setShowOrganization] = useState(false);
  const [showMetadata, setShowMetadata] = useState(false);
  const [showRelink, setShowRelink] = useState(false);
  const [showCopy, setShowCopy] = useState(false);
  const [showExport, setShowExport] = useState(false);
  const [copyRefreshing, setCopyRefreshing] = useState<{ catalog: string; stamp: string } | null>(null);
  const [previewEpoch, setPreviewEpoch] = useState(0);
  const [organizationScopeName, setOrganizationScopeName] = useState('');
  const [backupStatus, setBackupStatus] = useState<BackupStatus | null>(null);
  const [importStatus, setImportStatus] = useState<ImportStatus | null>(null);
  const [loading, setLoading] = useState(false);
  const [selected, setSelected] = useState<GridImage | null>(null);
  const selectedRef = useRef<GridImage | null>(null); selectedRef.current = selected;
  const [queue, setQueue] = useState<EditQueue | null>(null);
  const queueRef = useRef<EditQueue | null>(null);
  const [editor, setEditor] = useState<EditSnapshot | null>(null);
  const [error, setError] = useState('');
  const [busy, setBusy] = useState('');
  const [size, setSize] = useState(190);
  const [showFolders, setShowFolders] = useState(true);
  const [showInspector, setShowInspector] = useState(true);
  const [copyName, setCopyName] = useState<string | null>(null);
  const [history, setHistory] = useState<HistoryEntry[] | null>(null);
  const gate = useRef(new ActionGate());
  const [transitioning, setTransitioning] = useState(false);
  const perform = useCallback(async (action: () => Promise<void>) => {
    await gate.current.run(async () => {
      setTransitioning(true);
      try { await action(); } catch (e) { setError(errorText(e)); }
      finally { setTransitioning(false); }
    });
  }, []);
  const operationAbort = useRef<AbortController | null>(null);
  const catalog = status.catalog;
  const catalogRef = useRef(catalog); catalogRef.current = catalog;
  const storageRefresh = useRef(0);
  const storage = useRelink(catalog);
  const copies = useEditCopy(catalog);
  const outputs = usePhotoExport(catalog);
  const [exportPending, setExportPending] = useState<string | null>(null);
  const exportDirectHeld = catalog !== null && exportPending === catalog;
  const copyEditingHeld = copies.busy || copyRefreshing?.catalog === catalog;
  const copyRefreshed = useRef('');
  const storageWriteHold = storage.writeHeld || outputs.writeHeld || exportDirectHeld;
  const exportBlocked = exportDirectHeld || outputs.writeHeld || status.phase !== 'ready' || status.jobs_held || storage.writeHeld || copies.busy || copyRefreshing?.catalog === catalog;
  const exportBlockedRef = useRef(exportBlocked); exportBlockedRef.current = exportBlocked;
  const exportGate: ExportGate = async action => {
    let result!: Awaited<ReturnType<typeof action>>;
    await gate.current.afterCurrent(async () => {
      if (!catalog || catalogRef.current !== catalog || exportBlockedRef.current) throw new Error('This catalog is not ready for export admission.');
      setTransitioning(true);
      try {
        await queueRef.current?.flush();
        if (catalogRef.current !== catalog || exportBlockedRef.current) throw new Error('Catalog or write admission changed while saving edits.');
        result = await action();
      } finally { setTransitioning(false); }
    });
    return result;
  };
  const exportRefreshed = useRef('');
  useEffect(() => {
    const operation = outputs.operation;
    if (!catalog || !operation || !exportTerminal(operation)) return;
    const stamp = `${catalog}:${operation.id}`;
    if (exportRefreshed.current === stamp) return;
    exportRefreshed.current = stamp;
    const abort = new AbortController(), row = selectedRef.current;
    setRefresh(v => v + 1);
    if (row) void command({ command: 'image', args: { catalog, key: row.key } }, 'image', abort.signal).then(value => {
      if (!abort.signal.aborted && catalogRef.current === catalog && selectedRef.current === row && JSON.stringify(value) !== JSON.stringify(row)) setSelected(value);
    }).catch(e => { if (!abort.signal.aborted && catalogRef.current === catalog && selectedRef.current === row) setError(`Export operation finished; selected photo refresh failed: ${errorText(e)}`); });
    // Export does not change edit recipes. Keep the active EditQueue and any
    // pending draft intact while refreshing the guarded read-only image state.
    return () => abort.abort();
  }, [catalog, outputs.operation]);

  useEffect(() => {
    if (!desktopAvailable) return;
    let stopped = false;
    let quitting = false;
    const subscription = listen('catalog-close-requested', () => {
      if (quitting) return; quitting = true; setBusy('Saving changes and closing');
      void (async () => {
        try { await gate.current.afterCurrent(async () => { setTransitioning(true); try { await queueRef.current?.flush(); await invoke('catalog_quit'); } finally { setTransitioning(false); } }); }
        catch (e) { setError(errorText(e)); setBusy(''); quitting = false; }
      })();
    });
    void subscription.then(unlisten => { if (stopped) unlisten(); else void invoke('catalog_frontend_ready'); }).catch(e => setError(errorText(e)));
    return () => { stopped = true; void subscription.then(unlisten => unlisten()); };
  }, []);

  useEffect(() => {
    if (!desktopAvailable) return;
    let stopped = false;
    let timer: ReturnType<typeof setTimeout>;
    const poll = async () => {
      try { const next = await command({ command: 'status' }, 'status'); if (!stopped) setStatus(next); }
      catch (e) { if (!stopped) setError(errorText(e)); }
      if (!stopped) timer = setTimeout(() => { void poll(); }, 500);
    };
    void poll(); return () => { stopped = true; clearTimeout(timer); };
  }, []);
  useEffect(() => {
    queueRef.current = queue;
    if (!queue) { setEditor(null); return; }
    return queue.subscribe(setEditor);
  }, [queue]);

  const attachVariant = useCallback((variant: Variant) => {
    if (!catalog) return;
    const next = new EditQueue(variant, (base, recipe) => command({ command: 'save_recipe', args: { catalog, key: base.key, expected_revision: base.revision, recipe } }, 'variant'));
    queueRef.current = next; setQueue(next);
  }, [catalog]);
  const select = useCallback(async (row: GridImage) => {
    await perform(async () => {
      await queueRef.current?.flush();
      if (!catalog) return;
      const variant = await command({ command: 'variant', args: { catalog, key: row.key } }, 'variant');
      setSelected(row); attachVariant(variant); setError('');
    });
  }, [catalog, attachVariant, perform]);

  useEffect(() => {
    const operation = copies.operation;
    if (!catalog || !operation || !copyTerminal(operation) || operation.job.completed === '0') return;
    const stamp = `${catalog}:${operation.id}:${operation.job.completed}`;
    if (copyRefreshed.current === stamp) return;
    const hold = { catalog, stamp };
    let stopped = false; setCopyRefreshing(hold);
    void gate.current.afterCurrent(async () => {
      if (stopped || catalogRef.current !== catalog) return;
      const row = selectedRef.current, currentQueue = queueRef.current;
      let flushed = false;
      try {
        await currentQueue?.flush();
        flushed = true;
        if (row) {
          const variant = await command({ command: 'variant', args: { catalog, key: row.key } }, 'variant');
          if (!stopped && catalogRef.current === catalog && selectedRef.current === row && queueRef.current === currentQueue) attachVariant(variant);
        }
        if (!stopped) { copyRefreshed.current = stamp; setPreviewEpoch(v => v + 1); setRefresh(v => v + 1); }
      } catch (e) {
        if (!stopped && catalogRef.current === catalog) {
          setError(`${flushed ? 'Adjustment copy finished; reselect the photo to reload its settings' : 'Adjustment copy finished; pending edits could not be saved and remain in the editor'}: ${errorText(e)}`);
          if (flushed && queueRef.current === currentQueue) { queueRef.current = null; setQueue(null); }
        }
      } finally { setCopyRefreshing(value => value === hold ? null : value); }
    });
    return () => { stopped = true; setCopyRefreshing(value => value === hold ? null : value); };
  }, [catalog, copies.operation, attachVariant]);

  const organizationMutation = async (action: () => Promise<void>) => {
    if (storageWriteHold) throw new Error('Wait for the storage operation to finish before changing the catalog.');
    await gate.current.afterCurrent(async () => {
      setTransitioning(true);
      try {
        await queueRef.current?.flush();
        try { await action(); }
        finally { setCursor(null); setPrevious([]); setRefresh(value => value + 1); }
        if (catalog && selectedRef.current) {
          try { setSelected(await command({ command: 'image', args: { catalog, key: selectedRef.current.key } }, 'image')); }
          catch (e) { setError(`Change saved; selected photo refresh failed: ${errorText(e)}`); }
        }
      } finally { setTransitioning(false); }
    });
  };

  useEffect(() => {
    if (!catalog || status.phase !== 'ready' || scope === undefined) return;
    const abort = new AbortController(); setLoading(true);
    void command({ command: 'search', args: { catalog, options: { ...filters, folder: scope?.id ?? null, folder_recursive: recursive, text: appliedSearch || null }, cursor, limit: 100 } }, 'images', abort.signal)
      .then(next => { if (!abort.signal.aborted) { setPage(next); setError(''); } })
      .catch(e => { if (!abort.signal.aborted) setError(errorText(e)); })
      .finally(() => { if (!abort.signal.aborted) setLoading(false); });
    return () => abort.abort();
  }, [catalog, status.phase, scope, recursive, appliedSearch, filters, cursor, refresh]);

  const open = async (create: boolean) => perform(async () => {
    try {
      const backup = await command({ command: 'backup_status' }, 'backup');
      if (backup && ['running', 'cancel_requested'].includes(backup.state)) throw new Error('Finish or cancel the backup operation before opening another catalog.');
      const choice = await chooseFolder(create); if (!choice) return;
      const abort = new AbortController(); operationAbort.current = abort; setBusy(create ? 'Creating catalog' : 'Opening catalog');
      const next = await command({ command: create ? 'create' : 'open_existing', args: { path: choice.path } }, 'status', abort.signal);
      setStatus(next); setCatalogName(choice.display); setScope(undefined); setPage({ rows: [], next: null, has_more: false, page_complete: true, scanned: 0 });
      setSelected(null); queueRef.current = null; setQueue(null); setError(''); setShowOrganization(false); setShowExport(false); setFilters(defaultFilters); setOrganizationScopeName('');
    } catch (e) { setError(errorText(e)); } finally { operationAbort.current = null; setBusy(''); }
  });
  const close = async () => perform(async () => {
    if (!catalog) return;
    try { await queueRef.current?.flush(); setBusy('Closing catalog'); setStatus(await command({ command: 'close', args: { catalog } }, 'status')); setSelected(null); queueRef.current = null; setQueue(null); setImportStatus(null); setPage({ rows: [], next: null, has_more: false, page_complete: true, scanned: 0 }); }
    catch (e) { setError(errorText(e)); } finally { setBusy(''); }
  });
  const changeScope = async (next: Folder | null) => perform(async () => {
    try { await queueRef.current?.flush(); setScope(next); setCursor(null); setPrevious([]); setSelected(null); queueRef.current = null; setQueue(null); }
    catch (e) { setError(errorText(e)); }
  });
  const cull = useCallback(async (operation: CullOperation, advance: boolean) => perform(async () => {
    if (!catalog || !selected || storageWriteHold) return;
    try {
      await queueRef.current?.flush();
      const result = await command({ command: 'cull', args: { catalog, key: selected.key, expected_revision: selected.metadata_revision, operation } }, 'culled');
      const changed: GridImage = { ...selected, metadata_revision: result.metadata_revision,
        ...(operation.operation === 'rating' ? { rating: String(operation.value) } : operation.operation === 'flag' ? { flag: operation.value } : { label: operation.value }) };
      setSelected(changed); setPage(value => ({ ...value, rows: value.rows.map(row => row.image_id === changed.image_id ? changed : row) }));
      if (advance) { const at = page.rows.findIndex(row => row.image_id === selected.image_id); if (at >= 0 && at + 1 < page.rows.length) { const next = page.rows[at + 1]; const variant = await command({ command: 'variant', args: { catalog, key: next.key } }, 'variant'); setSelected(next); attachVariant(variant); } }
      setError('');
    } catch (e) { setError(errorText(e)); }
  }), [catalog, selected, page.rows, attachVariant, perform, storageWriteHold]);
  useEffect(() => {
    const keydown = (event: KeyboardEvent) => {
      if (document.querySelector('dialog[open]') || (event.target instanceof HTMLElement && (event.target.closest('input,textarea,select') || event.target.isContentEditable)) || event.metaKey || event.ctrlKey || event.altKey) return;
      if (!selected || event.repeat || gate.current.locked) return;
      if (/^[0-5]$/.test(event.key)) { event.preventDefault(); void cull({ operation: 'rating', value: Number(event.key) }, mode === 'cull'); }
      else if (['p', 'x', 'u'].includes(event.key.toLowerCase())) { event.preventDefault(); void cull({ operation: 'flag', value: event.key.toLowerCase() === 'p' ? 'pick' : event.key.toLowerCase() === 'x' ? 'reject' : 'unflagged' }, mode === 'cull'); }
      else if (mode !== 'library' && ['ArrowLeft', 'ArrowRight'].includes(event.key)) { const at = page.rows.findIndex(row => row.image_id === selected.image_id); const next = page.rows[at + (event.key === 'ArrowLeft' ? -1 : 1)]; if (next) { event.preventDefault(); void select(next); } }
    };
    window.addEventListener('keydown', keydown); return () => window.removeEventListener('keydown', keydown);
  }, [selected, cull, mode, page.rows, select]);
  const undo = async (redo: boolean) => perform(async () => {
    if (!catalog || !queue || storageWriteHold || copyEditingHeld) return;
    try { await queue.flush(); const value = queue.value.variant; attachVariant(await command({ command: redo ? 'redo' : 'undo', args: { catalog, key: value.key, expected_revision: value.revision } }, 'variant')); }
    catch (e) { setError(errorText(e)); }
  });
  const createCopy = async () => perform(async () => {
    if (!catalog || !queue || storageWriteHold || copyEditingHeld || copyName === null) return;
    try { await queue.flush(); const value = queue.value.variant; const variant = await command({ command: 'create_variant', args: { catalog, key: value.key, expected_revision: value.revision, label: copyName } }, 'variant'); const row = await command({ command: 'image', args: { catalog, key: variant.key } }, 'image'); setSelected(row); attachVariant(variant); setCopyName(null); setRefresh(value => value + 1); }
    catch (e) { setError(errorText(e)); }
  });
  const inspectHistory = async () => perform(async () => {
    if (!catalog || !editor) return;
    try { setHistory((await command({ command: 'history', args: { catalog, key: editor.variant.key, after: '0', limit: 100 } }, 'history')).rows); }
    catch (e) { setError(errorText(e)); }
  });

  return <div className="app-shell">
    <header className="app-header"><div className="wordmark"><span className="brand-mark" aria-hidden="true">▧</span>PhotoCatalog</div>
      <span className="catalog-name" title={catalogName}>{catalog ? catalogName.split(/[\\/]/).filter(Boolean).at(-1) || 'Catalog' : 'Local photo library'}</span>
      {catalog && <nav aria-label="Workspace">{(['library', 'cull', 'develop'] as const).map(value => <button key={value} aria-current={mode === value ? 'page' : undefined} onClick={() => setMode(value)}>{value}</button>)}</nav>}
      {desktopAvailable && <button className="quiet" disabled={transitioning} onClick={() => void perform(async () => { await queueRef.current?.flush(); setShowBackup(true); })}>Backups…</button>}
      {catalog && <button className="quiet" onClick={() => void close()} disabled={!!busy || transitioning}>Close catalog</button>}
    </header>
    {error && <ErrorNotice message={error} dismiss={() => setError('')} />}
    {busy && <div className="activity" role="status">{busy}… {operationAbort.current && <button onClick={() => operationAbort.current?.abort()}>Cancel</button>}</div>}
    {!catalog ? <main className="welcome"><div className="welcome-mark" aria-hidden="true">▧</div><h1>Your photographs.<br />One library.</h1><p>Keep every year together. Browse your folders, preserve your originals, and edit without losing where you started.</p>
      {desktopAvailable ? <div className="welcome-actions"><button className="primary" disabled={!!busy || transitioning} onClick={() => void open(false)}>Open catalog…</button><button disabled={!!busy || transitioning} onClick={() => void open(true)}>Create catalog…</button></div> : <p className="desktop-notice">Open the PhotoCatalog desktop app to create or open a catalog.</p>}
      <p className="hint">Catalogs store metadata and previews. Original photos stay in their existing folders, including external drives.</p>
    </main> : <>
      <div className="workspace-toolbar"><button disabled={transitioning || storageWriteHold || status.phase !== 'ready'} onClick={() => setShowImport(true)}>Add photos…</button><button onClick={() => void perform(async () => { await queueRef.current?.flush(); setShowRelink(true); })}>Locate originals…</button><button aria-pressed={showFolders} onClick={() => setShowFolders(value => !value)}>Folders</button><div className="breadcrumb">{scope === undefined ? 'Choose a folder' : scope === null ? 'All Photos' : scope.name}</div>
        <form className="search-form" onSubmit={event => { event.preventDefault(); setAppliedSearch(search); setCursor(null); setPrevious([]); }}><input type="search" maxLength={1024} aria-label="Search photos" placeholder="Search photos" value={search} onChange={event => setSearch(event.target.value)} /><button type="submit">Search</button></form>
        <button disabled={transitioning || storageWriteHold || status.phase !== 'ready'} onClick={() => void perform(async () => { await queueRef.current?.flush(); setShowOrganization(true); })}>Organize…</button><button disabled={transitioning} onClick={() => void perform(async () => { await queueRef.current?.flush(); setShowCopy(true); })}>Copy adjustments…</button><button disabled={transitioning} onClick={() => setShowExport(true)}>Export photos…</button><button onClick={() => setShowFilters(true)}>Filters…</button><button aria-pressed={showInspector} onClick={() => setShowInspector(value => !value)}>Inspector</button></div>
      {(filters.keyword || filters.collection) && <div className="activity">Organization filter: {organizationScopeName}<button onClick={() => { setFilters(value => ({ ...value, keyword: null, collection: null })); setCursor(null); setPrevious([]); }}>Clear organization filter</button></div>}
      {storage.writeHeld && <div className="activity" role="status">{storage.ready ? 'Storage operation in progress: catalog writes and new original rendering are held.' : 'Checking storage operation status; catalog writes are held.'}<button onClick={() => setShowRelink(true)}>Review progress</button>{storage.operation && !relinkTerminal(storage.operation) && <button onClick={() => void storage.cancel()}>Cancel storage operation</button>}</div>}
      {copies.busy && <div className="activity" role="status">{copies.ready ? 'Copying adjustments' : 'Checking adjustment copy status'} · {copies.operation?.job.completed ?? '0'} of {copies.operation?.job.total ?? '…'} targets processed<button onClick={() => setShowCopy(true)}>Review copy progress</button><button disabled={!copies.operation || copyTerminal(copies.operation)} onClick={() => void copies.cancel()}>Cancel adjustment copy</button></div>}
      {exportDirectHeld && <div className="activity" role="status">Export saved-job acknowledgement pending · catalog writes held. Inspect saved work or close and reopen this catalog.<button onClick={() => setShowExport(true)}>Review pending export write</button></div>}
      {(outputs.busy || outputs.operation) && <div className="activity" role="status">Photo export: {outputs.operation ? `${outputs.operation.kind.replaceAll('_', ' ')} · ${outputs.operation.stage.replaceAll('_', ' ')} · ${outputs.operation.phase.replaceAll('_', ' ')}` : 'checking operation status'}{outputs.writeHeld ? ' · catalog writes held' : ''}<button onClick={() => setShowExport(true)}>Review export</button>{outputs.operation && !exportTerminal(outputs.operation) && <><button onClick={() => void outputs.cancel().catch(e => setError(errorText(e)))}>Cancel export operation</button>{outputs.operation.kind === 'run' && <button onClick={() => void outputs.yield().catch(e => setError(errorText(e)))}>Pause export worker</button>}</>}{!outputs.ready && <button onClick={outputs.retry}>Retry export status</button>}</div>}
      {outputs.error && <ErrorNotice message={`Photo export: ${outputs.error}`} />}
      {copies.error && <div className="activity" role="alert">Adjustment copy: {copies.error}<button onClick={() => setShowCopy(true)}>Review copy status</button></div>}
      {status.jobs_held && <div className="activity">Restored catalog: pending external jobs are held for review. Browsing and editing are available.</div>}
      <main className="workspace">
        {showFolders && <aside className="left-panel"><Section title="Library"><button className={scope === null ? 'current scope-button' : 'scope-button'} onClick={() => void changeScope(null)}>All Photos</button><label className="checkbox"><input type="checkbox" checked={recursive} onChange={event => { setRecursive(event.target.checked); setCursor(null); setPrevious([]); }} />Include subfolders</label></Section><Section title="Folders">{status.phase === 'ready' ? <FolderTree key={`${catalog}:${folderEpoch}`} catalog={catalog} selected={scope?.id} onSelect={folder => void changeScope(folder)} /> : <p role="status">Preparing folders…</p>}</Section></aside>}
        <section className="central-panel" aria-label={`${mode} workspace`}>
          {status.phase !== 'ready' ? <CatalogActivity phase={status.phase} message={status.message} /> : mode === 'library' ? <>
            <div className="grid-toolbar"><span>{loading ? 'Loading photos…' : `${page.rows.length} photos on this page`}</span><label>Size<input aria-label="Thumbnail size" type="range" min="130" max="300" step="10" value={size} onChange={event => setSize(Number(event.target.value))} /></label></div>
            {scope === undefined ? <div className="empty-state"><h2>Choose a folder</h2><p>Your library follows the folders on disk. Select a folder, or open All Photos to browse across every year.</p></div> : page.rows.length ? <PhotoGrid key={`grid:${previewEpoch}`} edited={editor?.variant} catalog={catalog} rows={page.rows} selected={selected?.image_id ?? null} onSelect={row => void select(row)} onDevelop={() => setMode('develop')} size={size} /> : <div className="empty-state"><h2>{loading ? 'Loading photos…' : page.has_more ? 'More photos to check' : 'No photos in this view'}</h2><p>{page.has_more ? 'Continue to the next page to search the remaining candidates.' : 'Select another folder or change your search.'}</p></div>}
          </> : selected && editor ? <Viewport key={`viewport:${previewEpoch}`} catalog={catalog} image={selected} variant={editor.variant} interactive={mode === 'develop'} /> : <div className="empty-state"><h2>Select a photograph</h2><p>Choose a photo in Library to begin.</p><button onClick={() => setMode('library')}>Open Library</button></div>}
          {mode === 'cull' && selected && <div className="cull-bar"><button disabled={storageWriteHold} onClick={() => void cull({ operation: 'flag', value: 'pick' }, true)}>Pick <kbd>P</kbd></button><button disabled={storageWriteHold} onClick={() => void cull({ operation: 'flag', value: 'reject' }, true)}>Reject <kbd>X</kbd></button><button disabled={storageWriteHold} onClick={() => void cull({ operation: 'flag', value: 'unflagged' }, true)}>Unflag <kbd>U</kbd></button><span className="hint">0–5 rates · advances after saving</span></div>}
          {page.rows.length > 0 && <Filmstrip key={`filmstrip:${previewEpoch}`} edited={editor?.variant} catalog={catalog} rows={page.rows} selected={selected?.image_id ?? null} onSelect={row => void select(row)} onDevelop={() => setMode('develop')} />}
          <footer className="page-controls"><button disabled={loading || previous.length === 0} onClick={() => { setCursor(previous.at(-1)!); setPrevious(value => value.slice(0, -1)); }}>Previous</button><button disabled={loading || !page.has_more || !page.next} onClick={() => { setPrevious(value => [...value.slice(-7), cursor]); setCursor(page.next); }}>Next</button><button disabled={loading || scope === undefined} onClick={() => { setCursor(null); setPrevious([]); setRefresh(value => value + 1); }}>Refresh view</button><span>{selected?.filename || 'No selection'}</span></footer>
        </section>
        {showInspector && <aside className="right-panel">{selected && editor ? <>
          <Section title="Selected photo"><div className="selected-filename">{selected.filename}</div><CompatibilityStatus image={selected} selectedKey={editor.variant.key} />{editor.variant.label && <p className="hint">{editor.variant.label}</p>}<div className="rating-buttons" aria-label="Rating">{[0, 1, 2, 3, 4, 5].map(value => <button key={value} disabled={storageWriteHold} aria-label={`${value} stars`} aria-pressed={selected.rating === String(value)} onClick={() => void cull({ operation: 'rating', value }, false)}>{value === 0 ? '—' : '★'}</button>)}</div>
          <div className="button-group"><button disabled={storageWriteHold} aria-pressed={selected.flag === 'pick'} onClick={() => void cull({ operation: 'flag', value: selected.flag === 'pick' ? 'unflagged' : 'pick' }, false)}>Pick</button><button disabled={storageWriteHold} aria-pressed={selected.flag === 'reject'} onClick={() => void cull({ operation: 'flag', value: selected.flag === 'reject' ? 'unflagged' : 'reject' }, false)}>Reject</button></div>
          {selected.conflicts.length > 0 && <p className="hint">Conflicting metadata: {selected.conflicts.join(', ')}</p>}{selected.metadata_pending && <p className="hint">Metadata indexing is pending.</p>}<button disabled={transitioning} onClick={() => void perform(async () => { await queueRef.current?.flush(); setShowMetadata(true); })}>Metadata & XMP…</button></Section>
          {mode === 'develop' && <><div className="edit-status" role="status">{editor.state === 'saved' ? 'Changes saved' : editor.state === 'saving' ? 'Saving changes…' : editor.state === 'pending' ? 'Changes pending' : 'Changes could not be saved'}</div>{editor.error && <ErrorNotice message={editor.error} />}<RecipeControls disabled={transitioning || storageWriteHold || copyEditingHeld} recipe={editor.recipe} onChange={value => { if (!gate.current.locked && !storageWriteHold && !copyEditingHeld) queueRef.current?.change(value); }} />
          <div className="edit-actions"><button disabled={transitioning || storageWriteHold || copyEditingHeld || !editor.variant.can_undo} onClick={() => void undo(false)}>Undo</button><button disabled={transitioning || storageWriteHold || copyEditingHeld || !editor.variant.can_redo} onClick={() => void undo(true)}>Redo</button><button disabled={storageWriteHold || copyEditingHeld} onClick={() => setCopyName('Copy')}>Create variant…</button><button onClick={() => void inspectHistory()}>Edit history</button></div></>}
          {mode !== 'develop' && <Section title="Editing"><p className="hint">Changes apply to the selected photo or variant and leave the original untouched.</p><button onClick={() => setMode('develop')}>Open Develop</button></Section>}
        </> : <div className="empty-state"><p>Select a photo to inspect its metadata and edits.</p></div>}</aside>}
      </main><footer className="app-status"><span>{status.phase === 'ready' ? 'Catalog ready' : status.phase}</span><span>{importStatus && ['discovering', 'draining', 'cancel_requested'].includes(importStatus.phase) ? `Import ${importStatus.phase.replaceAll('_', ' ')} · ${importStatus.imported} added` : status.message}</span><span>{backupStatus && ['running', 'cancel_requested'].includes(backupStatus.state) ? `Backup ${backupStatus.state.replaceAll('_', ' ')}` : ''}</span><span>{status.active_previews > 0 ? `${status.active_previews} preview requests` : 'Local catalog'}</span></footer>
    </>}
    {catalog && <ExportPanel key={`export:${catalog}`} catalog={catalog} open={showExport} rows={page.rows} controller={outputs} gate={exportGate} blocked={exportBlocked} onDirectPending={pending => setExportPending(pending ? catalog : null)} onClose={() => setShowExport(false)} />}
    {catalog && <CopyPanel key={`copy:${catalog}`} catalog={catalog} open={showCopy} selected={selected} rows={page.rows} source={() => queueRef.current?.value.variant ?? null} controller={copies} mutate={organizationMutation} jobsHeld={status.jobs_held} writeHeld={storageWriteHold} onClose={() => setShowCopy(false)} />}
    {catalog && <RelinkPanel key={`relink:${catalog}`} catalog={catalog} selected={selected} open={showRelink} onClose={() => setShowRelink(false)} controller={storage} mutate={organizationMutation} changed={() => { setPreviewEpoch(v => v + 1); setFolderEpoch(v => v + 1); setCursor(null); setPrevious([]); setRefresh(v => v + 1); const selection = selectedRef.current; const generation = ++storageRefresh.current; const current = () => catalogRef.current === catalog && storageRefresh.current === generation && selectedRef.current === selection; if (selection) void command({ command: 'image', args: { catalog, key: selection.key } }, 'image').then(row => { if (current()) setSelected(row); }).catch(e => { if (current()) setError(errorText(e)); }); }} />}
    {catalog && <ImportPanel key={`import:${catalog}`} catalog={catalog} open={showImport} onProgress={setImportStatus} jobsHeld={status.jobs_held} onClose={() => setShowImport(false)} onComplete={() => { setFolderEpoch(value => value + 1); setCursor(null); setPrevious([]); setRefresh(value => value + 1); }} />}
    {desktopAvailable && <BackupPanel catalog={catalog} open={showBackup} onClose={() => setShowBackup(false)} onProgress={setBackupStatus} />}
    {catalog && selected && showMetadata && <MetadataPanel key={`${catalog}:${imageKey(selected.key)}`} catalog={catalog} variant={selected.key} filename={selected.filename} mutate={organizationMutation} onClose={() => setShowMetadata(false)} />}
    {catalog && <OrganizationPanel phase={status.phase} open={showOrganization} onOpen={() => setShowOrganization(true)} key={`organization:${catalog}`} catalog={catalog} selected={selected} selectedVariantLabel={editor?.variant.label ?? null} rows={page.rows} mutate={organizationMutation} onClose={() => setShowOrganization(false)} onSelect={async row => { await gate.current.afterCurrent(async () => { setTransitioning(true); try { await queueRef.current?.flush(); const variant = await command({ command: 'variant', args: { catalog, key: row.key } }, 'variant'); setSelected(row); attachVariant(variant); } finally { setTransitioning(false); } }); }} onFilter={(filter, name) => { setFilters(value => ({ ...value, ...filter })); setOrganizationScopeName(name); setScope(null); setCursor(null); setPrevious([]); setMode('library'); setShowOrganization(false); }} />}
    {showFilters && <SearchFilters value={filters} onApply={value => { setFilters(value); setCursor(null); setPrevious([]); }} onClose={() => setShowFilters(false)} />}
    {copyName !== null && <Dialog title="Create independent variant" onClose={() => setCopyName(null)}><p>Start a new edit from the current saved settings. The original and existing variant remain unchanged.</p><label className="form-field">Variant name<input autoFocus value={copyName} maxLength={256} onChange={event => setCopyName(event.target.value)} /></label><button className="primary" disabled={transitioning || !copyName.trim()} onClick={() => void createCopy()}>Create variant</button></Dialog>}
    {history && <Dialog title="Edit history" onClose={() => setHistory(null)}>{history.length ? <ol>{history.map(entry => <li key={entry.revision}><strong>Revision {entry.revision}</strong> · {entry.kind}</li>)}</ol> : <p>No saved history for this variant.</p>}</Dialog>}
  </div>;
}

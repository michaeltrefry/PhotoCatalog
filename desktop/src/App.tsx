import { useCallback, useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { command, chooseFolder, desktopAvailable, errorText, type CatalogStatus, type CullOperation, type Data, type Folder, type GridImage, type HistoryEntry, type Variant } from './bridge';
import { Dialog, ErrorNotice, Section } from './components/Controls';
import { FolderTree } from './components/FolderTree';
import { PhotoGrid, Filmstrip } from './components/PhotoGrid';
import { RecipeControls } from './components/RecipeControls';
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
  const [page, setPage] = useState<Data['images']>({ rows: [], next: null, has_more: false, page_complete: true, scanned: 0 });
  const [cursor, setCursor] = useState<string | null>(null);
  const [previous, setPrevious] = useState<(string | null)[]>([]);
  const [refresh, setRefresh] = useState(0);
  const [loading, setLoading] = useState(false);
  const [selected, setSelected] = useState<GridImage | null>(null);
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
    if (!catalog || status.phase !== 'ready' || scope === undefined) return;
    const abort = new AbortController(); setLoading(true);
    void command({ command: 'images', args: { catalog, folder: scope?.id ?? null, recursive, text: appliedSearch || null, cursor, limit: 100 } }, 'images', abort.signal)
      .then(next => { if (!abort.signal.aborted) { setPage(next); setError(''); } })
      .catch(e => { if (!abort.signal.aborted) setError(errorText(e)); })
      .finally(() => { if (!abort.signal.aborted) setLoading(false); });
    return () => abort.abort();
  }, [catalog, status.phase, scope, recursive, appliedSearch, cursor, refresh]);

  const open = async (create: boolean) => perform(async () => {
    try {
      const choice = await chooseFolder(create); if (!choice) return;
      const abort = new AbortController(); operationAbort.current = abort; setBusy(create ? 'Creating catalog' : 'Opening catalog');
      const next = await command({ command: create ? 'create' : 'open_existing', args: { path: choice.path } }, 'status', abort.signal);
      setStatus(next); setCatalogName(choice.display); setScope(undefined); setPage({ rows: [], next: null, has_more: false, page_complete: true, scanned: 0 });
      setSelected(null); queueRef.current = null; setQueue(null); setError('');
    } catch (e) { setError(errorText(e)); } finally { operationAbort.current = null; setBusy(''); }
  });
  const close = async () => perform(async () => {
    if (!catalog) return;
    try { await queueRef.current?.flush(); setBusy('Closing catalog'); setStatus(await command({ command: 'close', args: { catalog } }, 'status')); setSelected(null); queueRef.current = null; setQueue(null); setPage({ rows: [], next: null, has_more: false, page_complete: true, scanned: 0 }); }
    catch (e) { setError(errorText(e)); } finally { setBusy(''); }
  });
  const changeScope = async (next: Folder | null) => perform(async () => {
    try { await queueRef.current?.flush(); setScope(next); setCursor(null); setPrevious([]); setSelected(null); queueRef.current = null; setQueue(null); }
    catch (e) { setError(errorText(e)); }
  });
  const cull = useCallback(async (operation: CullOperation, advance: boolean) => perform(async () => {
    if (!catalog || !selected) return;
    try {
      await queueRef.current?.flush();
      const result = await command({ command: 'cull', args: { catalog, key: selected.key, expected_revision: selected.metadata_revision, operation } }, 'culled');
      const changed: GridImage = { ...selected, metadata_revision: result.metadata_revision,
        ...(operation.operation === 'rating' ? { rating: String(operation.value) } : operation.operation === 'flag' ? { flag: operation.value } : { label: operation.value }) };
      setSelected(changed); setPage(value => ({ ...value, rows: value.rows.map(row => row.image_id === changed.image_id ? changed : row) }));
      if (advance) { const at = page.rows.findIndex(row => row.image_id === selected.image_id); if (at >= 0 && at + 1 < page.rows.length) { const next = page.rows[at + 1]; const variant = await command({ command: 'variant', args: { catalog, key: next.key } }, 'variant'); setSelected(next); attachVariant(variant); } }
      setError('');
    } catch (e) { setError(errorText(e)); }
  }), [catalog, selected, page.rows, attachVariant, perform]);
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
    if (!catalog || !queue) return;
    try { await queue.flush(); const value = queue.value.variant; attachVariant(await command({ command: redo ? 'redo' : 'undo', args: { catalog, key: value.key, expected_revision: value.revision } }, 'variant')); }
    catch (e) { setError(errorText(e)); }
  });
  const createCopy = async () => perform(async () => {
    if (!catalog || !queue || copyName === null) return;
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
      {catalog && <button className="quiet" onClick={() => void close()} disabled={!!busy || transitioning}>Close catalog</button>}
    </header>
    {error && <ErrorNotice message={error} dismiss={() => setError('')} />}
    {busy && <div className="activity" role="status">{busy}… {operationAbort.current && <button onClick={() => operationAbort.current?.abort()}>Cancel</button>}</div>}
    {!catalog ? <main className="welcome"><div className="welcome-mark" aria-hidden="true">▧</div><h1>Your photographs.<br />One library.</h1><p>Keep every year together. Browse your folders, preserve your originals, and edit without losing where you started.</p>
      {desktopAvailable ? <div className="welcome-actions"><button className="primary" disabled={!!busy || transitioning} onClick={() => void open(false)}>Open catalog…</button><button disabled={!!busy || transitioning} onClick={() => void open(true)}>Create catalog…</button></div> : <p className="desktop-notice">Open the PhotoCatalog desktop app to create or open a catalog.</p>}
      <p className="hint">Catalogs store metadata and previews. Original photos stay in their existing folders, including external drives.</p>
    </main> : <>
      <div className="workspace-toolbar"><button aria-pressed={showFolders} onClick={() => setShowFolders(value => !value)}>Folders</button><div className="breadcrumb">{scope === undefined ? 'Choose a folder' : scope === null ? 'All Photos' : scope.name}</div>
        <form className="search-form" onSubmit={event => { event.preventDefault(); setAppliedSearch(search); setCursor(null); setPrevious([]); }}><input type="search" aria-label="Search photos" placeholder="Search photos" value={search} onChange={event => setSearch(event.target.value)} /><button type="submit">Search</button></form>
        <button aria-pressed={showInspector} onClick={() => setShowInspector(value => !value)}>Inspector</button></div>
      {status.jobs_held && <div className="activity">Restored catalog: pending external jobs are held for review. Browsing and editing are available.</div>}
      <main className="workspace">
        {showFolders && <aside className="left-panel"><Section title="Library"><button className={scope === null ? 'current scope-button' : 'scope-button'} onClick={() => void changeScope(null)}>All Photos</button><label className="checkbox"><input type="checkbox" checked={recursive} onChange={event => { setRecursive(event.target.checked); setCursor(null); setPrevious([]); }} />Include subfolders</label></Section><Section title="Folders">{status.phase === 'ready' ? <FolderTree key={catalog} catalog={catalog} selected={scope?.id} onSelect={folder => void changeScope(folder)} /> : <p role="status">Preparing folders…</p>}</Section></aside>}
        <section className="central-panel" aria-label={`${mode} workspace`}>
          {mode === 'library' ? <>
            <div className="grid-toolbar"><span>{loading ? 'Loading photos…' : `${page.rows.length} photos on this page`}</span><label>Size<input aria-label="Thumbnail size" type="range" min="130" max="300" step="10" value={size} onChange={event => setSize(Number(event.target.value))} /></label></div>
            {scope === undefined ? <div className="empty-state"><h2>Choose a folder</h2><p>Your library follows the folders on disk. Select a folder, or open All Photos to browse across every year.</p></div> : page.rows.length ? <PhotoGrid edited={editor?.variant} catalog={catalog} rows={page.rows} selected={selected?.image_id ?? null} onSelect={row => void select(row)} onDevelop={() => setMode('develop')} size={size} /> : <div className="empty-state"><h2>{loading ? 'Loading photos…' : page.has_more ? 'More photos to check' : 'No photos in this view'}</h2><p>{status.phase === 'indexing' ? 'Preparing the catalog index.' : page.has_more ? 'Continue to the next page to search the remaining candidates.' : 'Select another folder or change your search.'}</p></div>}
          </> : selected && editor ? <Viewport catalog={catalog} image={selected} variant={editor.variant} interactive={mode === 'develop'} /> : <div className="empty-state"><h2>Select a photograph</h2><p>Choose a photo in Library to begin.</p><button onClick={() => setMode('library')}>Open Library</button></div>}
          {mode === 'cull' && selected && <div className="cull-bar"><button onClick={() => void cull({ operation: 'flag', value: 'pick' }, true)}>Pick <kbd>P</kbd></button><button onClick={() => void cull({ operation: 'flag', value: 'reject' }, true)}>Reject <kbd>X</kbd></button><button onClick={() => void cull({ operation: 'flag', value: 'unflagged' }, true)}>Unflag <kbd>U</kbd></button><span className="hint">0–5 rates · advances after saving</span></div>}
          {page.rows.length > 0 && <Filmstrip edited={editor?.variant} catalog={catalog} rows={page.rows} selected={selected?.image_id ?? null} onSelect={row => void select(row)} onDevelop={() => setMode('develop')} />}
          <footer className="page-controls"><button disabled={loading || previous.length === 0} onClick={() => { setCursor(previous.at(-1)!); setPrevious(value => value.slice(0, -1)); }}>Previous</button><button disabled={loading || !page.has_more || !page.next} onClick={() => { setPrevious(value => [...value.slice(-7), cursor]); setCursor(page.next); }}>Next</button><button disabled={loading || scope === undefined} onClick={() => { setCursor(null); setPrevious([]); setRefresh(value => value + 1); }}>Refresh view</button><span>{selected?.filename || 'No selection'}</span></footer>
        </section>
        {showInspector && <aside className="right-panel">{selected && editor ? <>
          <Section title="Selected photo"><div className="selected-filename">{selected.filename}</div>{editor.variant.label && <p className="hint">{editor.variant.label}</p>}<div className="rating-buttons" aria-label="Rating">{[0, 1, 2, 3, 4, 5].map(value => <button key={value} aria-label={`${value} stars`} aria-pressed={selected.rating === String(value)} onClick={() => void cull({ operation: 'rating', value }, false)}>{value === 0 ? '—' : '★'}</button>)}</div>
          <div className="button-group"><button aria-pressed={selected.flag === 'pick'} onClick={() => void cull({ operation: 'flag', value: selected.flag === 'pick' ? 'unflagged' : 'pick' }, false)}>Pick</button><button aria-pressed={selected.flag === 'reject'} onClick={() => void cull({ operation: 'flag', value: selected.flag === 'reject' ? 'unflagged' : 'reject' }, false)}>Reject</button></div>
          {selected.conflicts.length > 0 && <p className="hint">Conflicting metadata: {selected.conflicts.join(', ')}</p>}{selected.metadata_pending && <p className="hint">Metadata indexing is pending.</p>}</Section>
          {mode === 'develop' && <><div className="edit-status" role="status">{editor.state === 'saved' ? 'Changes saved' : editor.state === 'saving' ? 'Saving changes…' : editor.state === 'pending' ? 'Changes pending' : 'Changes could not be saved'}</div>{editor.error && <ErrorNotice message={editor.error} />}<RecipeControls disabled={transitioning} recipe={editor.recipe} onChange={value => { if (!gate.current.locked) queueRef.current?.change(value); }} />
          <div className="edit-actions"><button disabled={transitioning || !editor.variant.can_undo} onClick={() => void undo(false)}>Undo</button><button disabled={transitioning || !editor.variant.can_redo} onClick={() => void undo(true)}>Redo</button><button onClick={() => setCopyName('Copy')}>Create variant…</button><button onClick={() => void inspectHistory()}>Edit history</button></div></>}
          {mode !== 'develop' && <Section title="Editing"><p className="hint">Changes apply to the selected photo or variant and leave the original untouched.</p><button onClick={() => setMode('develop')}>Open Develop</button></Section>}
        </> : <div className="empty-state"><p>Select a photo to inspect its metadata and edits.</p></div>}</aside>}
      </main><footer className="app-status"><span>{status.phase === 'ready' ? 'Catalog ready' : status.phase}</span><span>{status.message}</span><span>{status.active_previews > 0 ? `${status.active_previews} preview requests` : 'Local catalog'}</span></footer>
    </>}
    {copyName !== null && <Dialog title="Create independent variant" onClose={() => setCopyName(null)}><p>Start a new edit from the current saved settings. The original and existing variant remain unchanged.</p><label className="form-field">Variant name<input autoFocus value={copyName} maxLength={256} onChange={event => setCopyName(event.target.value)} /></label><button className="primary" disabled={transitioning || !copyName.trim()} onClick={() => void createCopy()}>Create variant</button></Dialog>}
    {history && <Dialog title="Edit history" onClose={() => setHistory(null)}>{history.length ? <ol>{history.map(entry => <li key={entry.revision}><strong>Revision {entry.revision}</strong> · {entry.kind}</li>)}</ol> : <p>No saved history for this variant.</p>}</Dialog>}
  </div>;
}

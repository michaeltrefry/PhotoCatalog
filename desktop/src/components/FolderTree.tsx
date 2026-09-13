import { useEffect, useState } from 'react';
import { command, errorText, type Folder } from '../bridge';

export function FolderTree({ catalog, selected, onSelect }: { catalog: string; selected: string | null | undefined; onSelect: (folder: Folder) => void }) {
  return <ul className="folder-tree" aria-label="Folders"><FolderChildren catalog={catalog} parent={null} selected={selected} onSelect={onSelect} /></ul>;
}
function FolderChildren({ catalog, parent, selected, onSelect }: { catalog: string; parent: string | null; selected: string | null | undefined; onSelect: (folder: Folder) => void }) {
  const [rows, setRows] = useState<Folder[]>([]);
  const [next, setNext] = useState<string | null>(null);
  const [after, setAfter] = useState('0');
  const [previous, setPrevious] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  useEffect(() => {
    const abort = new AbortController(); setBusy(true);
    void command({ command: 'folders', args: { catalog, parent, after, limit: 100 } }, 'folders', abort.signal)
      .then(page => { if (!abort.signal.aborted) { setRows(page.rows); setNext(page.next); setError(''); } })
      .catch(e => { if (!abort.signal.aborted) setError(errorText(e)); })
      .finally(() => { if (!abort.signal.aborted) setBusy(false); });
    return () => abort.abort();
  }, [catalog, parent, after]);
  return <>
    {rows.map(folder => <FolderNode key={folder.id} catalog={catalog} folder={folder} selected={selected} onSelect={onSelect} />)}
    {busy && <li className="hint" role="status">Loading folders…</li>}
    {error && <li className="hint" role="alert">{error}</li>}
    {after !== '0' && <li><button className="quiet" disabled={busy} onClick={() => { setAfter('0'); setPrevious([]); }}>First folders</button></li>}
    {previous.length > 0 && <li><button className="quiet" disabled={busy} onClick={() => { setAfter(previous.at(-1)!); setPrevious(value => value.slice(0, -1)); }}>Previous folders</button></li>}
    {next !== null && <li><button className="quiet" disabled={busy} onClick={() => { setPrevious(value => [...value.slice(-7), after]); setAfter(next); }}>Next folders</button></li>}
  </>;
}
function FolderNode({ catalog, folder, selected, onSelect }: { catalog: string; folder: Folder; selected: string | null | undefined; onSelect: (folder: Folder) => void }) {
  const [expanded, setExpanded] = useState(false);
  return <li>
    <div className={`folder-line ${selected === folder.id ? 'current' : ''}`}>
      <button className="folder-toggle" aria-label={`${expanded ? 'Collapse' : 'Expand'} ${folder.name}`} aria-expanded={expanded} onClick={() => setExpanded(value => !value)}>{expanded ? '▾' : '▸'}</button>
      <button className="folder-name" aria-current={selected === folder.id ? 'location' : undefined} title={folder.name} onClick={() => onSelect(folder)}>{folder.name}</button>
    </div>
    {expanded && <ul><FolderChildren catalog={catalog} parent={folder.id} selected={selected} onSelect={onSelect} /></ul>}
  </li>;
}

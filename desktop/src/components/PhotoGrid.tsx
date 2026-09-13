import { useEffect, useRef, useState } from 'react';
import { useVirtualizer } from '@tanstack/react-virtual';
import { imageKey, type GridImage, type Variant } from '../bridge';
import { usePreview } from '../state/usePreview';

export function PhotoTile({ catalog, row, selected, onSelect, onDevelop, compact = false, editRevision = '' }: {
  catalog: string; row: GridImage; selected: boolean; onSelect: () => void; onDevelop: () => void; compact?: boolean; editRevision?: string;
}) {
  const preview = usePreview(catalog, row.key, `${compact ? 'film' : 'grid'}-${row.image_id}`, false, `${row.metadata_revision}/${editRevision}`);
  const button = useRef<HTMLButtonElement>(null);
  return <button ref={button} className={`photo-tile ${selected ? 'selected' : ''} ${compact ? 'compact' : ''}`} aria-pressed={selected}
    aria-label={`${row.filename}${row.key.variant_id !== 'master' ? ', variant' : ''}${row.rating ? `, ${row.rating} stars` : ''}`}
    tabIndex={selected ? 0 : -1} onClick={onSelect} onDoubleClick={onDevelop} data-image={row.image_id}>
    <div className="photo-frame">
      {preview.url ? <img src={preview.url} alt="" draggable={false} /> : <span className="preview-placeholder">{preview.loading ? 'Loading preview' : preview.message || 'No preview'}</span>}
      {row.flag === 'pick' && <span className="photo-flag" title="Pick">⚑</span>}
      {row.flag === 'reject' && <span className="photo-flag" title="Reject">×</span>}
      {row.metadata_pending && <span className="photo-status" title="Metadata is being indexed">Indexing</span>}
    </div>
    {!compact && <div className="photo-caption"><span title={row.filename}>{row.filename}</span><span className="stars">{row.rating && Number(row.rating) > 0 ? '★'.repeat(Math.min(5, Number(row.rating))) : ''}</span></div>}
  </button>;
}

export function PhotoGrid({ catalog, rows, selected, onSelect, onDevelop, size = 190, edited }: {
  catalog: string; rows: GridImage[]; selected: string | null; onSelect: (row: GridImage) => void; onDevelop: () => void; size?: number; edited?: Variant;
}) {
  const parent = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(800);
  useEffect(() => {
    if (!parent.current) return;
    const observer = new ResizeObserver(entries => setWidth(entries[0].contentRect.width));
    observer.observe(parent.current); return () => observer.disconnect();
  }, []);
  const columns = Math.max(1, Math.floor((width - 24) / size));
  const height = Math.round((width - 24) / columns * 0.72) + 34;
  const virtual = useVirtualizer({ count: Math.ceil(rows.length / columns), getScrollElement: () => parent.current, estimateSize: () => height, overscan: 2 });
  useEffect(() => { virtual.measure(); }, [height, virtual]);
  useEffect(() => { const index = rows.findIndex(r => r.image_id === selected); if (index >= 0) virtual.scrollToIndex(Math.floor(index / columns), { align: 'auto' }); }, [selected, columns, rows, virtual]);
  const navigate = (key: string) => {
    const index = Math.max(0, rows.findIndex(r => r.image_id === selected));
    const delta = key === 'ArrowRight' ? 1 : key === 'ArrowLeft' ? -1 : key === 'ArrowDown' ? columns : key === 'ArrowUp' ? -columns : 0;
    if (!delta) return false;
    const next = rows[Math.min(rows.length - 1, Math.max(0, index + delta))];
    if (next) onSelect(next);
    return true;
  };
  return <div className="photo-grid" ref={parent} role="region" aria-label="Photos" tabIndex={selected ? -1 : 0}
    onFocus={e => { if (e.target === e.currentTarget && !selected && rows[0]) onSelect(rows[0]); }}
    onKeyDown={e => { if (navigate(e.key)) { e.preventDefault(); requestAnimationFrame(() => parent.current?.querySelector<HTMLButtonElement>('.selected')?.focus({ preventScroll: true })); } else if (e.key === 'Enter') onDevelop(); }}>
    <div style={{ height: virtual.getTotalSize(), position: 'relative' }}>{virtual.getVirtualItems().map(item => <div key={item.key} className="photo-row" style={{ height, gridTemplateColumns: `repeat(${columns}, minmax(0, 1fr))`, transform: `translateY(${item.start}px)` }}>
      {rows.slice(item.index * columns, (item.index + 1) * columns).map(row => <PhotoTile key={row.image_id} catalog={catalog} row={row} selected={selected === row.image_id} editRevision={edited && imageKey(edited.key) === imageKey(row.key) ? edited.revision : ''} onSelect={() => onSelect(row)} onDevelop={onDevelop} />)}
    </div>)}</div>
  </div>;
}

export function Filmstrip({ catalog, rows, selected, onSelect, onDevelop, edited }: { catalog: string; rows: GridImage[]; selected: string | null; onSelect: (row: GridImage) => void; onDevelop: () => void; edited?: Variant }) {
  const parent = useRef<HTMLDivElement>(null);
  const virtual = useVirtualizer({ horizontal: true, count: rows.length, getScrollElement: () => parent.current, estimateSize: () => 116, overscan: 2 });
  useEffect(() => { const index = rows.findIndex(r => r.image_id === selected); if (index >= 0) virtual.scrollToIndex(index, { align: 'auto' }); }, [selected, rows, virtual]);
  return <div className="filmstrip" ref={parent} role="region" aria-label="Filmstrip"><div style={{ width: virtual.getTotalSize(), height: 78, position: 'relative' }}>{virtual.getVirtualItems().map(item => <div key={item.key} style={{ position: 'absolute', width: 112, height: 76, transform: `translateX(${item.start}px)` }}>
    <PhotoTile compact catalog={catalog} row={rows[item.index]} selected={rows[item.index].image_id === selected} editRevision={edited && imageKey(edited.key) === imageKey(rows[item.index].key) ? edited.revision : ''} onSelect={() => onSelect(rows[item.index])} onDevelop={onDevelop} />
  </div>)}</div></div>;
}

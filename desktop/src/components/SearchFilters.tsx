import { useState } from 'react';
import type { SearchOptions } from '../bridge';
import { Dialog } from './Controls';

export const defaultFilters: SearchOptions = { sort: 'sequence', direction: 'ascending', only_conflicted: false };
export function SearchFilters({ value, onApply, onClose }: { value: SearchOptions; onApply: (value: SearchOptions) => void; onClose: () => void }) {
  const [draft, setDraft] = useState(value);
  const text = (key: 'date_from' | 'date_until' | 'camera_make' | 'camera' | 'lens' | 'format' | 'label', label: string, type = 'text') => <label className="form-field">{label}<input type={type} value={draft[key] ?? ''} maxLength={1024} onChange={event => setDraft(value => ({ ...value, [key]: event.target.value || null }))} /></label>;
  return <Dialog title="Search filters" onClose={onClose}><form onSubmit={event => { event.preventDefault(); onApply(draft); onClose(); }}>
    <div className="filter-fields">{text('date_from', 'Captured from (inclusive)', 'date')}{text('date_until', 'Captured before (exclusive)', 'date')}{text('camera_make', 'Camera make')}{text('camera', 'Camera model')}{text('lens', 'Lens')}{text('format', 'Format')}{text('label', 'Color label')}
    <label className="form-field">Rating<select value={draft.rating ?? ''} onChange={event => setDraft(value => ({ ...value, rating: event.target.value === '' ? null : Number(event.target.value) }))}><option value="">Any rating</option>{[0, 1, 2, 3, 4, 5].map(value => <option key={value} value={value}>{value} stars</option>)}</select></label>
    <label className="form-field">Flag<select value={draft.flag ?? ''} onChange={event => setDraft(value => ({ ...value, flag: (event.target.value || null) as SearchOptions['flag'] }))}><option value="">Any flag</option><option value="pick">Pick</option><option value="reject">Reject</option><option value="unflagged">Unflagged</option></select></label>
    <label className="form-field">Sort by<select value={draft.sort} onChange={event => setDraft(value => ({ ...value, sort: event.target.value as SearchOptions['sort'] }))}><option value="sequence">Catalog order</option><option value="capture">Capture date</option><option value="filename">Filename</option><option value="rating">Rating</option></select></label>
    <label className="form-field">Direction<select value={draft.direction} onChange={event => setDraft(value => ({ ...value, direction: event.target.value as SearchOptions['direction'] }))}><option value="ascending">Ascending</option><option value="descending">Descending</option></select></label></div>
    <label className="checkbox"><input type="checkbox" checked={draft.only_conflicted ?? false} onChange={event => setDraft(value => ({ ...value, only_conflicted: event.target.checked }))} />Only photos with metadata conflicts</label>
    <p className="hint">Field filters match cataloged values. Unrated and conflicting ratings remain distinct from zero stars.</p>
    <div className="button-group"><button className="primary" type="submit">Apply filters</button><button type="button" onClick={() => setDraft(defaultFilters)}>Reset filters</button></div>
  </form></Dialog>;
}

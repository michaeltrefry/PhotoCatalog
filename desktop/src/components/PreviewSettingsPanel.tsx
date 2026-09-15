import { useEffect, useRef, useState } from 'react';
import { chooseLocation, displayPath, errorText, type NativePath } from '../bridge';
import { reviewedBudget, previewSettings, type PreviewSettingsStatus, type PreviewTier } from '../previewSettings';
import { Dialog, ErrorNotice } from './Controls';

export function PreviewSettingsPanel({ catalog, open, onClose }: { catalog: string; open: boolean; onClose: () => void }) {
  const [status, setStatus] = useState<PreviewSettingsStatus | null>(null);
  const [thumbnail, setThumbnail] = useState(''); const [large, setLarge] = useState('');
  const [roots, setRoots] = useState<NativePath[]>([]);
  const [tier, setTier] = useState<PreviewTier>('large');
  const [destination, setDestination] = useState<{ path: NativePath; display: string } | null>(null);
  const [error, setError] = useState(''); const [busy, setBusy] = useState(false);
  const alive = useRef(true); const working = useRef(false); const pause = useRef(false); const pauseReview = useRef(false);
  const pickerEpoch = useRef(0);
  useEffect(() => { alive.current = true; return () => { alive.current = false; pause.current = true; ++pickerEpoch.current; }; }, []);
  useEffect(() => { ++pickerEpoch.current; if (!open) pauseReview.current = true; }, [open]);
  const adopt = (value: PreviewSettingsStatus) => {
    if (!alive.current) return;
    setStatus(value);
    setThumbnail((BigInt(value.thumbnail_bytes) / 1048576n).toString());
    setLarge((BigInt(value.large_bytes) / 1048576n).toString());
    setRoots(value.original_roots.roots);
  };
  const perform = async (action: () => Promise<void>) => {
    if (working.current) return;
    working.current = true; setBusy(true); setError('');
    try { await action(); } catch (e) { if (alive.current) { setError(`${errorText(e)} Refresh storage status before continuing.`); setStatus(null); } }
    finally { working.current = false; if (alive.current) setBusy(false); }
  };
  useEffect(() => { if (open) void perform(async () => adopt(await previewSettings(catalog, { command: 'status' }))); }, [open, catalog]);
  const move = async () => {
    pause.current = false;
    let current = await previewSettings(catalog, { command: 'status' }); adopt(current);
    while (alive.current && !pause.current && current.relocation_pending && current.relocation_tier) {
      current = await previewSettings(catalog, { command: 'step_relocation', args: { tier: current.relocation_tier, objects: 10, bytes: '134217728' } });
      adopt(current);
    }
  };
  const reviewRoots = async (initial?: PreviewSettingsStatus) => {
    pauseReview.current = false;
    let current = initial ?? await previewSettings(catalog, { command: 'begin_original_root_review', args: { roots } });
    adopt(current);
    while (alive.current && !pauseReview.current && current.original_roots.state === 'reviewing' && current.original_roots.review) {
      current = await previewSettings(catalog, { command: 'step_original_root_review', args: { review: current.original_roots.review, directories: 100 } });
      adopt(current);
    }
  };
  const pathKey = (path: NativePath) => JSON.stringify(path);
  return <>
    {!open && busy && <div className="activity" role="status">Updating preview storage…<button onClick={() => { pause.current = true; pauseReview.current = true; }}>Pause after current batch</button></div>}
    {open && <Dialog title="Preview storage" onClose={onClose}>
      <p>Original photos stay in their folders. These settings control retained thumbnails and larger previews for this catalog.</p>
      {error && <ErrorNotice message={error} />}
      <button disabled={busy} onClick={() => void perform(async () => adopt(await previewSettings(catalog, { command: 'status' })))}>Refresh storage status</button>
      {status && <><p>Thumbnails: {displayPath(status.thumbnail_root)}</p><p>Larger previews: {displayPath(status.large_root)}</p>
        <label className="form-field">Retained thumbnail budget (MiB)<input inputMode="numeric" value={thumbnail} disabled={busy} onChange={e => setThumbnail(e.target.value)} /></label>
        <label className="form-field">Larger-preview budget (MiB)<input inputMode="numeric" value={large} disabled={busy} onChange={e => setLarge(e.target.value)} /></label>
        <p className="hint">Reducing the thumbnail budget preserves existing thumbnails and can pause new publication. Larger previews use the configured cache budget.</p>
        <button disabled={busy} onClick={() => void perform(async () => adopt(await previewSettings(catalog, { command: 'set_budgets', args: { thumbnail_bytes: reviewedBudget(thumbnail, status.thumbnail_bytes), large_bytes: reviewedBudget(large, status.large_bytes) } })))}>Save budgets</button>
        <section aria-labelledby="original-root-review-title"><h3 id="original-root-review-title">Original-photo roots</h3>
          <p>LensWorks checks every cataloged photo folder against these boundaries before it can move previews. Choose the folders you originally imported, such as the single folder containing all year folders. Broad roots are conservative and may prevent preview storage anywhere beneath them.</p>
          {roots.length ? <ul>{roots.map(root => <li key={pathKey(root)}>{displayPath(root)} <button disabled={busy || status.original_roots.state === 'reviewing'} onClick={() => setRoots(value => value.filter(item => pathKey(item) !== pathKey(root)))}>Remove</button></li>)}</ul> : <p role="status">No original-photo roots have been reviewed for this catalog.</p>}
          {status.original_roots.uncovered && <p>The catalog also contains photos under {displayPath(status.original_roots.uncovered)}.</p>}
          {status.original_roots.message && <p className="hint">{status.original_roots.message}</p>}
          <button disabled={busy || status.original_roots.state === 'reviewing'} onClick={() => { const epoch = ++pickerEpoch.current; void chooseLocation('original_root').then(value => { if (value && alive.current && epoch === pickerEpoch.current) setRoots(current => current.some(root => pathKey(root) === pathKey(value.path)) ? current : [...current, value.path]); }).catch(e => { if (alive.current && epoch === pickerEpoch.current) setError(errorText(e)); }); }}>Add original-photo root…</button>
          {status.original_roots.state === 'reviewing' ? <><p role="status">Verified {status.original_roots.checked_directories} catalog photo folders so far.</p><button disabled={busy} onClick={() => void perform(async () => reviewRoots(status))}>Continue verification</button><button disabled={!busy} onClick={() => { pauseReview.current = true; }}>Pause after current batch</button></> : <button disabled={busy || roots.length === 0} onClick={() => void perform(() => reviewRoots())}>{status.original_roots.state === 'ready' ? 'Verify these roots again' : 'Verify these roots'}</button>}
          {status.original_roots.state === 'ready' && <p role="status">Original-photo roots are verified for the catalog’s current locations.</p>}
        </section>
        {status.relocation_pending ? <><p role="status">{busy ? 'Moving' : 'Paused move of'} {status.relocation_tier === 'thumbnail' ? 'thumbnails' : 'larger previews'}.</p>
          {status.relocation && <><p>{status.relocation.phase === 'copy' ? 'Copying and verifying. The previous location is still in use.' : 'Copy verified. The new location is in use; removing verified old copies.'}</p><p>From {displayPath(status.relocation.source)} to {displayPath(status.relocation.destination)}</p>{status.relocation.objects !== null ? <p role="status">{status.relocation.objects} of {status.relocation.total_objects} previews · {status.relocation.bytes} of {status.relocation.total_bytes} bytes in this phase.</p> : <p>Saved progress from an earlier version is available. Object counts were not recorded for this move.</p>}</>}
          <button disabled={busy} onClick={() => void perform(move)}>Resume move</button><button disabled={!busy} onClick={() => { pause.current = true; }}>Pause after current batch</button></>
          : <><label className="form-field">Move<select value={tier} disabled={busy} onChange={e => { setTier(e.target.value as PreviewTier); setDestination(null); ++pickerEpoch.current; }}><option value="large">Larger previews</option><option value="thumbnail">Retained thumbnails</option></select></label>
            <button disabled={busy} onClick={() => { const epoch = ++pickerEpoch.current; void chooseLocation('preview_destination').then(value => { if (alive.current && epoch === pickerEpoch.current) setDestination(value); }).catch(e => { if (alive.current && epoch === pickerEpoch.current) setError(errorText(e)); }); }}>Choose new location…</button>
            {destination && <><p>{destination.display}</p><p>Use a new, empty folder outside your original-photo folders. Moving copies and verifies previews before switching locations; a paused move resumes from its saved progress.</p><button disabled={busy || status.original_roots.state !== 'ready'} onClick={() => void perform(async () => { adopt(await previewSettings(catalog, { command: 'begin_relocation', args: { tier, destination: destination.path } })); setDestination(null); await move(); })}>Move previews to this location</button>{status.original_roots.state !== 'ready' && <p className="hint">Verify the original-photo roots above before starting this move.</p>}</>}
          </>}
      </>}
      {busy && <p role="status">Updating preview storage…</p>}
    </Dialog>}
  </>;
}

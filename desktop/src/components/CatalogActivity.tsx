import { useEffect, useState } from 'react';
import type { CatalogStatus } from '../bridge';

export function CatalogActivity({ phase, message }: Pick<CatalogStatus, 'phase' | 'message'>) {
  const [seconds, setSeconds] = useState(0);
  useEffect(() => {
    const started = Date.now();
    setSeconds(0);
    const timer = setInterval(() => setSeconds(Math.floor((Date.now() - started) / 1000)), 1000);
    return () => clearInterval(timer);
  }, [phase]);
  const failed = phase === 'failed';
  return <div className="empty-state catalog-activity" role={failed ? 'alert' : 'status'} aria-busy={!failed}>
    {!failed && <span className="activity-spinner" aria-hidden="true" />}
    <h2>{failed ? 'Catalog could not finish loading' : phase === 'opening' ? 'Opening catalog…' : phase === 'closing' ? 'Closing catalog…' : 'Preparing your catalog…'}</h2>
    <p>{failed ? message || 'Close the catalog and try opening it again.' : phase === 'indexing' ? 'Updating the catalog’s metadata and folder index. Your photos will appear here when it is ready.' : 'Please wait while the catalog operation finishes.'}</p>
    {!failed && <p className="hint" aria-live="off">{seconds < 60 ? `${seconds} seconds elapsed` : `${Math.floor(seconds / 60)}m ${seconds % 60}s elapsed`}{phase === 'indexing' && ' · Large catalogs can take longer on their first open.'}</p>}
  </div>;
}

import { useState } from 'react';
import { type GridImage, type Variant } from '../bridge';
import { CompatibilityStatus } from './CompatibilityStatus';
import { usePreview } from '../state/usePreview';

export function Viewport({ catalog, image, variant, interactive }: { catalog: string; image: GridImage; variant: Variant; interactive: boolean }) {
  const [fit, setFit] = useState(true);
  const [attempt, setAttempt] = useState(0);
  const preview = usePreview(catalog, variant.key, 'current-photo', true, variant.revision, interactive, attempt);
  return <div className="viewport-panel">
    <div className="viewport-toolbar"><span>{image.filename}{variant.label ? ` · ${variant.label}` : ''}</span><div className="button-group"><button aria-pressed={fit} onClick={() => setFit(true)}>Fit</button><button aria-pressed={!fit} onClick={() => setFit(false)}>100% preview</button></div></div>
    <div className={`viewport ${fit ? 'fit' : 'actual'}`} tabIndex={0} aria-label="Image viewport">
      {preview.url ? <img src={preview.url} alt={`${image.filename}, current saved edit`} draggable={false} /> : <div className="empty-state">{preview.loading && <span className="activity-spinner" aria-hidden="true" />}<p role="status">{preview.message || (preview.loading ? 'Preparing preview…' : 'Preview unavailable')}</p>{!preview.loading && <button onClick={() => setAttempt(value => value + 1)}>Retry preview</button>}</div>}
    </div>
    <CompatibilityStatus image={image} selectedKey={variant.key} className="viewport-note" />
    <p className="viewport-note">{interactive ? 'Interactive proxy · full-quality export uses the original' : 'Catalog preview'}</p>
  </div>;
}

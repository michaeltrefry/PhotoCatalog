import { useState } from 'react';
import { type GridImage, type Variant } from '../bridge';
import { usePreview } from '../state/usePreview';

export function Viewport({ catalog, image, variant, interactive }: { catalog: string; image: GridImage; variant: Variant; interactive: boolean }) {
  const [fit, setFit] = useState(true);
  const preview = usePreview(catalog, variant.key, 'current-photo', true, variant.revision, interactive);
  return <div className="viewport-panel">
    <div className="viewport-toolbar"><span>{image.filename}{variant.label ? ` · ${variant.label}` : ''}</span><div className="button-group"><button aria-pressed={fit} onClick={() => setFit(true)}>Fit</button><button aria-pressed={!fit} onClick={() => setFit(false)}>100% preview</button></div></div>
    <div className={`viewport ${fit ? 'fit' : 'actual'}`} tabIndex={0} aria-label="Image viewport">
      {preview.url ? <img src={preview.url} alt={`${image.filename}, current saved edit`} draggable={false} /> : <div className="empty-state"><p role="status">{preview.loading ? 'Preparing preview…' : preview.message || 'Preview unavailable'}</p></div>}
    </div>
    <p className="viewport-note">{interactive ? 'Interactive proxy · full-quality export uses the original' : 'Catalog preview'}</p>
  </div>;
}

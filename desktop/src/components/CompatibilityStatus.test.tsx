import { describe, expect, it } from 'vitest';
import { renderToStaticMarkup } from 'react-dom/server';
import { CompatibilityStatus, compatibilityMessage } from './CompatibilityStatus';
const master = { asset_id: 'shared-original', variant_id: 'master' };
const copy = { ...master, variant_id: 'imported-virtual-copy' };
const image = (translation_state: string, key = copy, origin = 'import') => ({ key, origin, translation_state });
describe('selected logical image preview compatibility', () => {
  it('keeps native images free of an Adobe compatibility badge', () => {
    expect(renderToStaticMarkup(<CompatibilityStatus image={image('native', master, 'native')} selectedKey={master} />)).toBe('');
    expect(compatibilityMessage(image('native'), copy)).toBeNull();
  });
  it('explains translation without claiming Lightroom appearance parity', () => {
    expect(compatibilityMessage(image('translated'), copy)).toBe('Lightroom: supported settings translated; appearance may differ.');
    expect(compatibilityMessage(image('retained_only'), copy)).toBe('Adobe settings retained; this native preview does not reproduce them.');
    expect(compatibilityMessage(image('untranslated'), copy)).toBe('Imported settings not translated.');
  });
  it('does not borrow a master or another variant’s compatibility state', () => {
    const mismatch = compatibilityMessage(image('translated', master), copy);
    expect(mismatch).toBe('Compatibility status is unavailable for this variant.');
    expect(mismatch).not.toContain('supported settings translated');
    expect(compatibilityMessage(image('retained_only'), copy)).toContain('does not reproduce');
    expect(compatibilityMessage(image('native', master, 'native'), master)).toBeNull();
  });
  it('keeps unknown states truthful and renders ordinary accessible text', () => {
    const html = renderToStaticMarkup(<CompatibilityStatus image={image('future_state')} selectedKey={copy} />);
    expect(html).toContain('role="note"'); expect(html).toContain('compatibility is unknown'); expect(html).toContain('appearance is not verified');
    expect(compatibilityMessage(image('future_state', copy, 'future_origin'), copy)).toBe('Preview compatibility is unknown for this image.');
  });
});

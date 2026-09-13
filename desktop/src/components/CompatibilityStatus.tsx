import { imageKey, type GridImage, type VariantKey } from '../bridge';

type CompatibilityImage = Pick<GridImage, 'key' | 'origin' | 'translation_state'>;
export function compatibilityMessage(image: CompatibilityImage, selectedKey: VariantKey): string | null {
  if (imageKey(image.key) !== imageKey(selectedKey)) return 'Compatibility status is unavailable for this variant.';
  switch (image.translation_state) {
    case 'native': return null;
    case 'translated': return 'Lightroom: supported settings translated; appearance may differ.';
    case 'retained_only': return 'Adobe settings retained; this native preview does not reproduce them.';
    case 'untranslated': return 'Imported settings not translated.';
    default: return image.origin === 'import' ? 'Imported settings compatibility is unknown; Lightroom appearance is not verified.' : 'Preview compatibility is unknown for this image.';
  }
}
export function CompatibilityStatus({ image, selectedKey, className = 'hint' }: { image: CompatibilityImage; selectedKey: VariantKey; className?: string }) {
  const message = compatibilityMessage(image, selectedKey);
  return message ? <p className={className} role="note" aria-label="Preview compatibility">{message}</p> : null;
}

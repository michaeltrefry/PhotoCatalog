import { imageKey, type GridImage, type VariantKey } from './bridge';

export const adjustmentGroups = [
  ['geometry', 'Crop & straighten'], ['exposure', 'Exposure'],
  ['white_balance', 'White balance'], ['tone', 'Tone'], ['color', 'Color'],
  ['sharpening', 'Sharpening'], ['noise_reduction', 'Noise reduction'],
] as const;
export type { AdjustmentGroup } from './recipe';

/** Selection is limited to the displayed page; saved batches can span pages. */
export function copyCandidates(rows: GridImage[], source: VariantKey | null): GridImage[] {
  const seen = new Set<string>();
  const sourceId = source ? imageKey(source) : null;
  return rows.filter(row => {
    const id = imageKey(row.key);
    if (id === sourceId || seen.has(id)) return false;
    seen.add(id); return true;
  });
}

export function chosenCopyTargets(rows: GridImage[], chosen: ReadonlySet<string>, source: VariantKey | null): GridImage[] {
  return copyCandidates(rows, source).filter(row => chosen.has(imageKey(row.key)));
}

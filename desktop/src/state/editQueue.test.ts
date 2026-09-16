import { afterEach, expect, test, vi } from 'vitest';
import { EditQueue } from './editQueue';
import type { Variant } from '../bridge';

const variant: Variant = {
  key: { asset_id: 'asset', variant_id: 'copy' }, label: 'Copy', revision: '9007199254740993', recipe_digest: 'initial', can_undo: false, can_redo: false,
  recipe: { version: '1', settings: { crop: null, straighten_degrees: 0, exposure_ev: 0, white_balance: { mode: 'as_shot' }, contrast: 0, highlights: 0, shadows: 0, saturation: 0, vibrance: 0, sharpening: { amount: 0, radius_px: 1 }, noise_reduction: { luminance: 0, chroma: 0 } } },
};
const exposure = (value: number) => ({ ...variant.recipe, settings: { ...variant.recipe.settings, exposure_ev: value } });
afterEach(() => vi.useRealTimers());

test('a slider move during an in-flight save uses its returned exact revision', async () => {
  vi.useFakeTimers();
  const writes: { revision: string; exposure: number }[] = [];
  let finish!: (value: Variant) => void;
  const first = new Promise<Variant>(resolve => { finish = resolve; });
  const queue = new EditQueue(variant, async (base, recipe) => {
    writes.push({ revision: base.revision, exposure: recipe.settings.exposure_ev });
    if (writes.length === 1) return first;
    return { ...base, revision: '9007199254740995', recipe };
  });
  queue.change(exposure(1));
  const flushed = queue.flush();
  queue.change(exposure(2));
  finish({ ...variant, revision: '9007199254740994', recipe: exposure(1) });
  await flushed;
  expect(writes).toEqual([{ revision: '9007199254740993', exposure: 1 }, { revision: '9007199254740994', exposure: 2 }]);
  expect(queue.value.state).toBe('saved');
  expect(queue.value.recipe.settings.exposure_ev).toBe(2);
});

test('a revision conflict preserves draft and rejects navigation flush', async () => {
  vi.useFakeTimers();
  const save = vi.fn().mockRejectedValue(new Error('Revision changed'));
  const queue = new EditQueue(variant, save);
  queue.change(exposure(3));
  await expect(queue.flush()).rejects.toThrow('Revision changed');
  expect(queue.value.state).toBe('error');
  expect(queue.value.recipe.settings.exposure_ev).toBe(3);
  expect(queue.value.variant.revision).toBe(variant.revision);
  expect(save).toHaveBeenCalledTimes(1);
});

import { afterEach, expect, test, vi } from 'vitest';
import { EDIT_BUSY_RETRY_BASE_MS, EDIT_BUSY_RETRY_LIMIT, EDIT_BUSY_RETRY_MAX_MS, EDIT_COALESCE_MS, EditQueue } from './editQueue';
import type { Variant } from '../bridge';

const variant: Variant = {
  key: { asset_id: 'asset', variant_id: 'copy' }, label: 'Copy', revision: '9007199254740993', recipe_digest: 'initial', can_undo: false, can_redo: false,
  recipe: { version: '1', settings: { crop: null, straighten_degrees: 0, exposure_ev: 0, white_balance: { mode: 'as_shot' }, contrast: 0, highlights: 0, shadows: 0, saturation: 0, vibrance: 0, sharpening: { amount: 0, radius_px: 1 }, noise_reduction: { luminance: 0, chroma: 0 } } },
};
const exposure = (value: number) => ({ ...variant.recipe, settings: { ...variant.recipe.settings, exposure_ev: value } });
class RejectedBeforeMutation extends Error {}
const retryable = (error: unknown) => error instanceof RejectedBeforeMutation;
const fullRetryDelay = () => Array.from(
  { length: EDIT_BUSY_RETRY_LIMIT },
  (_, index) => Math.min(EDIT_BUSY_RETRY_BASE_MS * 2 ** index, EDIT_BUSY_RETRY_MAX_MS),
).reduce((sum, value) => sum + value, 0);
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

test('the one-frame coalescing window saves only the latest recipe and reports superseded input', async () => {
  vi.useFakeTimers();
  const events: { ordinal: number; outcome: string; revision?: string }[] = [];
  const save = vi.fn(async (base: Variant, recipe: Variant['recipe']) => ({ ...base, revision: '9007199254740994', recipe }));
  const queue = new EditQueue(variant, save, event => events.push(event));
  queue.change(exposure(1), 10);
  await vi.advanceTimersByTimeAsync(EDIT_COALESCE_MS - 1);
  expect(save).not.toHaveBeenCalled();
  queue.change(exposure(2), 11);
  expect(events).toEqual([{ ordinal: 10, outcome: 'superseded' }]);
  await vi.advanceTimersByTimeAsync(EDIT_COALESCE_MS);
  expect(save).toHaveBeenCalledTimes(1);
  expect(save.mock.calls[0][1].settings.exposure_ev).toBe(2);
  expect(events).toEqual([{ ordinal: 10, outcome: 'superseded' }, { ordinal: 11, outcome: 'durable', revision: '9007199254740994' }]);
});

test('a measured edit made during an in-flight save remains serialized on the returned revision', async () => {
  vi.useFakeTimers();
  const events: { ordinal: number; outcome: string; revision?: string }[] = [];
  const writes: string[] = [];
  let finish!: (value: Variant) => void;
  const first = new Promise<Variant>(resolve => { finish = resolve; });
  const queue = new EditQueue(variant, async (base, recipe) => {
    writes.push(base.revision);
    return writes.length === 1 ? first : { ...base, revision: '9007199254740995', recipe };
  }, event => events.push(event));
  queue.change(exposure(1), 20);
  const flushing = queue.flush();
  queue.change(exposure(2), 21);
  finish({ ...variant, revision: '9007199254740994', recipe: exposure(1) });
  await flushing;
  expect(writes).toEqual(['9007199254740993', '9007199254740994']);
  expect(events).toEqual([{ ordinal: 20, outcome: 'superseded' }, { ordinal: 21, outcome: 'durable', revision: '9007199254740995' }]);
});

test('measurement failure cannot change a successful edit', async () => {
  vi.useFakeTimers();
  const queue = new EditQueue(variant, async (base, recipe) => ({ ...base, revision: '9007199254740994', recipe }), () => { throw new Error('measurement failed'); });
  queue.change(exposure(4), 30);
  await expect(queue.flush()).resolves.toBeUndefined();
  expect(queue.value.state).toBe('saved');
  expect(queue.value.variant.revision).toBe('9007199254740994');
});

test('a pre-mutation hold waits in the same measured write and then becomes durable', async () => {
  vi.useFakeTimers();
  const events: { ordinal: number; outcome: string; revision?: string }[] = [];
  let attempts = 0;
  const save = vi.fn(async (base: Variant, recipe: Variant['recipe']) => {
    attempts += 1;
    if (attempts <= 2) throw new RejectedBeforeMutation('export write hold');
    return { ...base, revision: '9007199254740994', recipe };
  });
  const queue = new EditQueue(variant, save, event => events.push(event), retryable);
  queue.change(exposure(5), 40);
  const flushed = queue.flush();
  await vi.advanceTimersByTimeAsync(EDIT_BUSY_RETRY_BASE_MS);
  expect(events).toEqual([]);
  expect(queue.value.state).toBe('saving');
  await vi.advanceTimersByTimeAsync(EDIT_BUSY_RETRY_BASE_MS * 2);
  await flushed;
  expect(save).toHaveBeenCalledTimes(3);
  expect(events).toEqual([{ ordinal: 40, outcome: 'durable', revision: '9007199254740994' }]);
  expect(queue.value.state).toBe('saved');
});

test('a newer slider recipe supersedes the held recipe without starting a parallel write', async () => {
  vi.useFakeTimers();
  const events: { ordinal: number; outcome: string; revision?: string }[] = [];
  const writes: number[] = [];
  const save = vi.fn(async (base: Variant, recipe: Variant['recipe']) => {
    writes.push(recipe.settings.exposure_ev);
    if (writes.length === 1) throw new RejectedBeforeMutation('export write hold');
    return { ...base, revision: '9007199254740994', recipe };
  });
  const queue = new EditQueue(variant, save, event => events.push(event), retryable);
  queue.change(exposure(1), 41);
  const firstFlush = queue.flush();
  const sameFlush = queue.flush();
  await vi.advanceTimersByTimeAsync(0);
  queue.change(exposure(2), 42);
  await vi.advanceTimersByTimeAsync(EDIT_BUSY_RETRY_BASE_MS);
  await Promise.all([firstFlush, sameFlush]);
  expect(writes).toEqual([1, 2]);
  expect(events).toEqual([
    { ordinal: 41, outcome: 'superseded' },
    { ordinal: 42, outcome: 'durable', revision: '9007199254740994' },
  ]);
  expect(queue.value.recipe.settings.exposure_ev).toBe(2);
});

test('a persistent pre-mutation hold has a finite retry budget and reports one terminal failure', async () => {
  vi.useFakeTimers();
  const events: { ordinal: number; outcome: string }[] = [];
  const save = vi.fn().mockRejectedValue(new RejectedBeforeMutation('export write hold remained active'));
  const queue = new EditQueue(variant, save, event => events.push(event), retryable);
  queue.change(exposure(6), 43);
  const rejected = expect(queue.flush()).rejects.toThrow('export write hold remained active');
  await vi.advanceTimersByTimeAsync(fullRetryDelay());
  await rejected;
  expect(save).toHaveBeenCalledTimes(EDIT_BUSY_RETRY_LIMIT + 1);
  expect(events).toEqual([{ ordinal: 43, outcome: 'backend_error' }]);
  expect(queue.value.state).toBe('error');
  expect(vi.getTimerCount()).toBe(0);
});

test('an ambiguous transport error mentioning busy is never retried', async () => {
  vi.useFakeTimers();
  const save = vi.fn().mockRejectedValue(new Error('transport failed while export was busy'));
  const queue = new EditQueue(variant, save, undefined, retryable);
  queue.change(exposure(7));
  await expect(queue.flush()).rejects.toThrow('transport failed while export was busy');
  expect(save).toHaveBeenCalledTimes(1);
  expect(queue.value.state).toBe('error');
  expect(vi.getTimerCount()).toBe(0);
});

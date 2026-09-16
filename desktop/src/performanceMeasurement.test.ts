import { expect, test, vi } from 'vitest';
import { PerformanceRecorder, ReceiptFinalizer } from './performanceMeasurement';

const context = { duringImport: false, duringExport: false };

function harness(max = 8) {
  let now = 0;
  const frames: FrameRequestCallback[] = [];
  const recorder = new PerformanceRecorder('run', max, { now: () => now, timeOrigin: 1234 }, callback => { frames.push(callback); return frames.length; });
  return {
    recorder,
    advance: (milliseconds: number) => { now += milliseconds; },
    frame: () => { const callbacks = frames.splice(0); callbacks.forEach(callback => callback(now)); },
  };
}

test('durable cull and presentation opportunity use the same input ordinal', () => {
  const h = harness();
  h.advance(2);
  const ordinal = h.recorder.begin('cull', { duringImport: true, duringExport: true })!;
  h.advance(4); h.recorder.durable(ordinal);
  h.advance(2); h.recorder.present(ordinal, () => true);
  h.frame(); h.advance(10); h.frame();
  expect(h.recorder.receipt().samples).toEqual([expect.objectContaining({
    kind: 'cull', ordinal, started_us: 2000, during_import: true, during_export: true, outcome: 'complete', durable_us: 4000, presentation_us: 16000,
  })]);
});

test('a post-commit failure cannot revoke a durable cull presentation', () => {
  const h = harness();
  const ordinal = h.recorder.begin('cull', context)!;
  h.advance(4); h.recorder.durable(ordinal);
  h.recorder.present(ordinal, () => true);
  h.recorder.end(ordinal, 'backend_error');
  h.frame(); h.advance(12); h.frame();
  expect(h.recorder.receipt().samples).toEqual([expect.objectContaining({
    kind: 'cull', outcome: 'complete', durable_us: 4000, presentation_us: 16000,
  })]);
});

test('browse completes only after the frozen visible roster is decoded and presented', () => {
  const h = harness();
  const ordinal = h.recorder.begin('browse', context)!;
  h.advance(3); h.recorder.searchResponse(ordinal, 100);
  h.recorder.thumbnailDecoded(ordinal, 'second', () => true);
  h.frame(); h.advance(5); h.frame();
  h.recorder.visible(ordinal, ['first', 'second']);
  h.advance(2); h.recorder.thumbnailDecoded(ordinal, 'first', () => true);
  h.frame(); h.advance(7); h.frame();
  expect(h.recorder.receipt().samples).toEqual([expect.objectContaining({
    kind: 'browse', outcome: 'complete', search_response_us: 3000, first_thumbnail_us: 8000,
    visible_complete_us: 17000, page_rows: 100, visible_count: 2,
  })]);
});

test('sample and active bounds fail closed without growing the receipt', () => {
  const h = harness(2);
  expect(h.recorder.begin('edit', context)).toBe(1);
  expect(h.recorder.begin('edit', context)).toBe(2);
  expect(h.recorder.begin('edit', context)).toBeUndefined();
  const receipt = h.recorder.receipt();
  expect(receipt.samples).toHaveLength(2);
  expect(receipt.samples.every(sample => sample.outcome === 'incomplete')).toBe(true);
  expect(receipt.overflowed).toBe(1);
});

test('finalization freezes one snapshot, coalesces submission, and retries the same receipt', async () => {
  const h = harness();
  h.recorder.begin('edit', context);
  const submitted: unknown[] = [];
  let reject!: (reason: unknown) => void;
  const first = new Promise<string>((_, rejectAttempt) => { reject = rejectAttempt; });
  const submit = vi.fn((receipt: unknown) => {
    submitted.push(receipt);
    return submitted.length === 1 ? first : Promise.resolve('/receipt.json');
  });
  const finalizer = new ReceiptFinalizer(h.recorder, submit);
  const left = finalizer.finish(), right = finalizer.finish();
  expect(left).toBe(right);
  expect(submit).toHaveBeenCalledTimes(1);
  expect(h.recorder.begin('cull', context)).toBeUndefined();
  reject(new Error('temporary write failure'));
  await expect(left).rejects.toThrow('temporary write failure');
  await expect(finalizer.finish()).resolves.toBe('/receipt.json');
  expect(submit).toHaveBeenCalledTimes(2);
  expect(submitted[1]).toBe(submitted[0]);
});

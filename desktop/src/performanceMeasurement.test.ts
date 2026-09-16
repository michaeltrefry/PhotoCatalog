import { expect, test, vi } from 'vitest';
import { classifyThumbnailPresentation, PerformanceRecorder, ReceiptFinalizer } from './performanceMeasurement';

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

test('animation frames retain the browser receiver after the scheduler is stored', () => {
  let now = 0;
  const frames: FrameRequestCallback[] = [];
  function receiverSensitiveFrame(this: typeof globalThis, callback: FrameRequestCallback) {
    if (this !== globalThis) throw new TypeError('Illegal invocation');
    frames.push(callback);
    return frames.length;
  }
  const recorder = new PerformanceRecorder(
    'run', 8, { now: () => now, timeOrigin: 1234 }, receiverSensitiveFrame,
  );
  const ordinal = recorder.begin('cull', context)!;
  recorder.present(ordinal, () => true);
  frames.splice(0).forEach(callback => callback(now));
  now = 5;
  frames.splice(0).forEach(callback => callback(now));
  expect(recorder.receipt().samples).toEqual([
    expect.objectContaining({ kind: 'cull', outcome: 'complete', presentation_us: 5000 }),
  ]);
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
  h.recorder.thumbnailAttempt();
  h.recorder.thumbnailDecoded(ordinal, 'second', () => 'accepted');
  h.frame(); h.advance(5); h.frame();
  h.recorder.visible(ordinal, ['first', 'second']);
  h.advance(2); h.recorder.thumbnailAttempt(); h.recorder.thumbnailDecoded(ordinal, 'first', () => 'accepted');
  h.frame(); h.advance(7); h.frame();
  expect(h.recorder.receipt().samples).toEqual([expect.objectContaining({
    kind: 'browse', outcome: 'complete', search_response_us: 3000, first_thumbnail_us: 8000,
    visible_complete_us: 17000, page_rows: 100, visible_count: 2,
  })]);
  expect(h.recorder.receipt().thumbnail_diagnostics).toEqual(expect.objectContaining({ attempts: 2, accepted: 2, roster_tiles: 2 }));
});

test('thumbnail diagnostics distinguish every presentation rejection without completing the roster', () => {
  const h = harness();
  const ordinal = h.recorder.begin('browse', context)!;
  h.recorder.searchResponse(ordinal, 100);
  h.recorder.visible(ordinal, ['tile']);
  h.recorder.thumbnailAttempt(); h.recorder.thumbnailDecodeFailed();
  h.recorder.thumbnailDecoded(ordinal, 'tile', () => 'source_changed');
  h.frame(); h.frame();
  const receipt = h.recorder.receipt();
  expect(receipt.samples).toEqual([expect.objectContaining({ kind: 'browse', outcome: 'incomplete', visible_count: 1 })]);
  expect(receipt.thumbnail_diagnostics).toEqual({
    attempts: 1, decode_completed: 0, decode_failed: 1, source_changed: 1,
    disconnected: 0, incomplete: 0, zero_size: 0, nonvisible: 0,
    accepted: 0, roster_tiles: 1, pending_expected: 1,
  });
});

test('accepted diagnostics count unique expected tiles and retain pending roster size', () => {
  const h = harness();
  const ordinal = h.recorder.begin('browse', context)!;
  h.recorder.searchResponse(ordinal, 100);
  h.recorder.visible(ordinal, ['first', 'second']);
  h.recorder.thumbnailAttempt(); h.recorder.thumbnailDecodeCompleted();
  h.recorder.thumbnailDecoded(ordinal, 'first', () => 'accepted');
  h.recorder.thumbnailDecoded(ordinal, 'first', () => 'accepted');
  h.frame(); h.frame();
  const receipt = h.recorder.receipt();
  expect(receipt.thumbnail_diagnostics).toEqual(expect.objectContaining({
    attempts: 1, decode_completed: 1, accepted: 1, roster_tiles: 2, pending_expected: 1,
  }));
});

test('thumbnail presentation classifier keeps current decoded visible element gates explicit', () => {
  const image = {
    getAttribute: () => 'blob:current', isConnected: true, complete: true,
    naturalWidth: 100, naturalHeight: 80,
    getBoundingClientRect: () => ({ width: 100, height: 80, top: 1, left: 1, bottom: 81, right: 101, x: 1, y: 1, toJSON: () => ({}) }),
  } as unknown as HTMLImageElement;
  Object.defineProperty(globalThis, 'window', { configurable: true, value: { innerHeight: 600, innerWidth: 800 } });
  expect(classifyThumbnailPresentation(image, 'blob:current')).toBe('accepted');
  expect(classifyThumbnailPresentation({ ...image, getAttribute: () => 'blob:stale' }, 'blob:current')).toBe('source_changed');
  expect(classifyThumbnailPresentation({ ...image, isConnected: false }, 'blob:current')).toBe('disconnected');
  expect(classifyThumbnailPresentation({ ...image, complete: false }, 'blob:current')).toBe('incomplete');
  expect(classifyThumbnailPresentation({ ...image, naturalWidth: 0 }, 'blob:current')).toBe('zero_size');
  expect(classifyThumbnailPresentation({ ...image, getBoundingClientRect: () => ({ width: 100, height: 80, top: 700, left: 1, bottom: 780, right: 101, x: 1, y: 700, toJSON: () => ({}) }) }, 'blob:current')).toBe('nonvisible');
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

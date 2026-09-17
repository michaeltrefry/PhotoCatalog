import { expect, test, vi } from 'vitest';
import { classifyThumbnailPresentation, PerformanceRecorder, ReceiptFinalizer } from './performanceMeasurement';
import type { ImportStatus } from './bridge';

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

const importStatus = (phase: ImportStatus['phase'], imported = '0'): ImportStatus => ({
  id: '00000000-0000-4000-8000-000000000001',
  source: { encoding: 'UnixBytes', units: [47, 115, 111, 117, 114, 99, 101] },
  phase, imported, unchanged: '0', failed: '0', skipped: '0', metadata_updated: imported,
  metadata_warnings: '0', awaiting_resources: '0', pending_previews: 0, error: null, error_source: null,
});

test('fulfilled import status starts the import profile and binds terminal progress once per source', () => {
  const h = harness();
  h.recorder.enableClockAlignment(() => new Promise(() => {}));
  const warmup = h.recorder.begin('edit', context)!;
  h.advance(1); h.recorder.durable(warmup); h.recorder.present(warmup, () => true); h.frame(); h.frame();
  expect(h.recorder.observeImportStatus(h.recorder.beginImportStatusRequest()!, importStatus('complete'))).toBe(false);
  expect(h.recorder.observeImportStatus(h.recorder.beginImportStatusRequest()!, importStatus('discovering'))).toBe(true);
  const ordinal = h.recorder.begin('cull', { duringImport: true, importId: importStatus('discovering').id, duringExport: false })!;
  h.advance(3); h.recorder.durable(ordinal); h.recorder.present(ordinal, () => true); h.frame(); h.frame();
  expect(h.recorder.observeImportStatus(h.recorder.beginImportStatusRequest()!, importStatus('complete', '1'))).toBe(true);
  const receipt = h.recorder.receipt();
  expect(receipt.samples[0]).toMatchObject({ durable_us: 1000 });
  expect(receipt.samples[0]).not.toHaveProperty('import_id');
  expect(receipt.samples[1]).toMatchObject({ import_id: importStatus('complete').id, durable_us: 3000 });
  expect(receipt.clock_alignment).toMatchObject({
    profile: 'import_v1',
    sample_events: [
      { ordinal: warmup },
      { ordinal, durable_event: expect.any(Number) },
    ],
    import_evidence: {
      bindings: [{ key: 1, id: importStatus('complete').id, source_blake3: 'ea86888b76029916e2b607c2f2d661c840f9707799dd41a113da27f8ca282ce1' }],
      timeline: [expect.objectContaining({ binding: 1, phase: 'discovering', imported: '0' }), expect.objectContaining({ binding: 1, phase: 'complete', imported: '1' })],
      overflowed: 0,
    },
  });
  expect(receipt.clock_alignment?.sample_events[0]).not.toHaveProperty('durable_event');
});

test('more than four import identities overflow once without retaining later jobs', () => {
  const h = harness();
  h.recorder.enableClockAlignment(() => new Promise(() => {}));
  const statusFor = (index: number, phase: ImportStatus['phase'] = 'discovering') => ({
    ...importStatus(phase), id: `00000000-0000-4000-8000-${String(index).padStart(12, '0')}`,
  });
  for (let index = 1; index <= 4; index += 1) {
    expect(h.recorder.observeImportStatus(h.recorder.beginImportStatusRequest()!, statusFor(index))).toBe(true);
  }
  expect(h.recorder.observeImportStatus(h.recorder.beginImportStatusRequest()!, {
    ...statusFor(5), source: { encoding: 'UnixBytes', units: [] },
  })).toBe(false);
  expect(h.recorder.observeImportStatus(h.recorder.beginImportStatusRequest()!, statusFor(5))).toBe(false);
  expect(h.recorder.observeImportStatus(h.recorder.beginImportStatusRequest()!, statusFor(5, 'complete'))).toBe(false);
  expect(h.recorder.observeImportStatus(h.recorder.beginImportStatusRequest()!, statusFor(6))).toBe(false);
  expect(h.recorder.receipt().clock_alignment?.import_evidence).toMatchObject({
    bindings: expect.arrayContaining([expect.objectContaining({ id: statusFor(4).id })]),
    overflowed: 1,
  });
  expect(h.recorder.receipt().clock_alignment?.import_evidence?.bindings).toHaveLength(4);
  expect(h.recorder.receipt().clock_alignment?.import_evidence?.timeline).toHaveLength(4);
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

function scrollSurface(overrides: Partial<HTMLElement> = {}) {
  return {
    isConnected: true, scrollTop: 0, scrollLeft: 0,
    clientWidth: 800, clientHeight: 600, scrollWidth: 800, scrollHeight: 2400,
    ...overrides,
  } as unknown as HTMLElement;
}

test('scroll cadence retains the receiver-sensitive native callback timestamp and surface proof', () => {
  let now = 20, timer: (() => void) | undefined;
  const frames: FrameRequestCallback[] = [];
  const target = scrollSurface();
  function receiverSensitiveFrame(this: typeof globalThis, callback: FrameRequestCallback) {
    if (this !== globalThis) throw new TypeError('Illegal invocation');
    frames.push(callback); return frames.length;
  }
  const recorder = new PerformanceRecorder(
    'run', 8, { now: () => now, timeOrigin: 1234 }, receiverSensitiveFrame, () => {}, () => {},
    callback => { timer = callback; return 1 as unknown as ReturnType<typeof setTimeout>; }, () => {}, () => target,
  );
  expect(recorder.startScroll(target)).toBe(true);
  frames.splice(0).forEach(callback => callback(19.99));
  target.scrollTop = 500;
  frames.splice(0).forEach(callback => callback(5019));
  now = 5020; timer!();
  expect(recorder.receipt().scroll_capture).toEqual(expect.objectContaining({
    outcome: 'complete', reason: 'duration_elapsed', started_us: 20_000, ended_us: 5_020_000,
    target_initial: expect.objectContaining({ identity: 1, scroll_top_px: 0, scroll_height_px: 2400 }),
    target_final: expect.objectContaining({ identity: 1, scroll_top_px: 500, scroll_height_px: 2400 }),
    frames: [[19_990, 0, 0], [5_019_000, 500, 0]],
  }));
});

test('scroll cadence stops bounded and incomplete when its mounted grid disappears', () => {
  let now = 0;
  let connected = true;
  const frames: FrameRequestCallback[] = [];
  const target = scrollSurface();
  Object.defineProperty(target, 'isConnected', { get: () => connected });
  const recorder = new PerformanceRecorder(
    'run', 8, { now: () => now, timeOrigin: 1234 }, callback => { frames.push(callback); return frames.length; }, () => {}, () => {},
    () => 1 as unknown as ReturnType<typeof setTimeout>, () => {}, () => connected ? target : null,
  );
  expect(recorder.startScroll(target)).toBe(true);
  frames.splice(0).forEach(callback => callback(5));
  connected = false;
  now = 10;
  frames.splice(0).forEach(callback => callback(10));
  expect(recorder.scrollState).toBe('incomplete');
  expect(recorder.receipt().scroll_capture).toEqual(expect.objectContaining({
    outcome: 'incomplete', reason: 'unmounted', target_final: null,
    frames: [[5000, 0, 0], [10_000, 0, 0]],
  }));
  expect(recorder.receipt().samples).toEqual([]);
});

test('duration timer retains an unmount reason when no later animation frame arrives', () => {
  let now = 0, timer: (() => void) | undefined;
  let connected = true;
  const target = scrollSurface();
  Object.defineProperty(target, 'isConnected', { get: () => connected });
  const recorder = new PerformanceRecorder(
    'run', 8, { now: () => now, timeOrigin: 1234 }, () => 1, () => {}, () => {},
    callback => { timer = callback; return 1 as unknown as ReturnType<typeof setTimeout>; }, () => {}, () => connected ? target : null,
  );
  expect(recorder.startScroll(target)).toBe(true);
  connected = false;
  now = 5_000;
  timer!();
  expect(recorder.receipt().scroll_capture).toEqual(expect.objectContaining({
    outcome: 'incomplete', reason: 'unmounted', target_final: null, frames: [],
  }));
});

test('finalizing an active scroll capture preserves callbacks and marks it incomplete', () => {
  const frames: FrameRequestCallback[] = [];
  const target = scrollSurface();
  const recorder = new PerformanceRecorder(
    'run', 8, { now: () => 20, timeOrigin: 1234 }, callback => { frames.push(callback); return frames.length; }, () => {}, () => {},
    () => 1 as unknown as ReturnType<typeof setTimeout>, () => {}, () => target,
  );
  recorder.startScroll(target);
  frames.splice(0).forEach(callback => callback(20));
  expect(recorder.receipt().scroll_capture).toEqual(expect.objectContaining({
    outcome: 'incomplete', reason: 'finalized', frames: [[20_000, 0, 0]],
  }));
});

test('scroll cadence stops at its fixed frame bound without scheduling another callback', () => {
  let now = 0;
  const frames: FrameRequestCallback[] = [];
  const target = scrollSurface();
  const recorder = new PerformanceRecorder(
    'run', 8, { now: () => now, timeOrigin: 1234 }, callback => { frames.push(callback); return frames.length; }, () => {}, () => {},
    () => 1 as unknown as ReturnType<typeof setTimeout>, () => {}, () => target,
  );
  recorder.startScroll(target);
  for (let index = 0; index < 2048; index += 1) {
    now = index;
    expect(frames).toHaveLength(1);
    frames.splice(0).forEach(callback => callback(index));
  }
  expect(frames).toHaveLength(0);
  const capture = recorder.receipt().scroll_capture!;
  expect(capture).toEqual(expect.objectContaining({ outcome: 'incomplete', reason: 'frame_limit' }));
  expect(capture.frames).toHaveLength(2048);
});

test('a delayed duration timer is retained as an incomplete observation', () => {
  let now = 0, timer: (() => void) | undefined;
  const frames: FrameRequestCallback[] = [];
  const target = scrollSurface();
  const recorder = new PerformanceRecorder(
    'run', 8, { now: () => now, timeOrigin: 1234 }, callback => { frames.push(callback); return frames.length; }, () => {}, () => {},
    callback => { timer = callback; return 1 as unknown as ReturnType<typeof setTimeout>; }, () => {}, () => target,
  );
  recorder.startScroll(target);
  frames.splice(0).forEach(callback => callback(1));
  frames.splice(0).forEach(callback => callback(17));
  now = 12_000; timer!();
  expect(recorder.receipt().scroll_capture).toEqual(expect.objectContaining({
    outcome: 'incomplete', reason: 'duration_elapsed', ended_us: 12_000_000,
    frames: [[1000, 0, 0], [17_000, 0, 0]],
  }));
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

import { afterEach, expect, test, vi } from 'vitest';
import { ANCHOR_DURATION_MS, MeasurementClock, type NativeAnchor } from './measurementClock';
import { PerformanceRecorder } from './performanceMeasurement';

const native = (id: number): NativeAnchor => ({ run_id: 'run', anchor_id: id, session_id: 'session', native_pid: 42, clock: 'macos_mach_absolute_ns', monotonic_ns: String(9_007_199_254_741_000n + BigInt(id)) });
afterEach(() => vi.useRealTimers());

test('causal send/receive brackets preserve lossless native time and sample ordering', async () => {
  vi.useFakeTimers();
  const clock = new MeasurementClock(async id => native(id), 'run');
  clock.start(); await Promise.resolve();
  clock.begin(1); clock.end(1);
  await vi.advanceTimersByTimeAsync(100);
  const value = clock.receipt();
  expect(value.anchors).toHaveLength(2);
  expect(value.anchors[0].receive_event).toBeLessThan(value.sample_events[0].start_event);
  expect(value.anchors[1].send_event).toBeGreaterThan(value.sample_events[0].end_event!);
  expect(value.anchors[0].native?.monotonic_ns).toBe('9007199254741001');
});

test('single in-flight request survives repeated starts and is bounded by duration', async () => {
  vi.useFakeTimers();
  let resolve!: (value: NativeAnchor) => void;
  const request = vi.fn(() => new Promise<NativeAnchor>(done => { resolve = done; }));
  const clock = new MeasurementClock(request, 'run');
  clock.start(); clock.start();
  await vi.advanceTimersByTimeAsync(ANCHOR_DURATION_MS + 1);
  expect(request).toHaveBeenCalledTimes(1);
  clock.start(); resolve(native(1)); await Promise.resolve();
  await vi.advanceTimersByTimeAsync(1000);
  expect(request).toHaveBeenCalledTimes(1);
  expect(clock.receipt().stop_reason).toBe('duration_elapsed');
});

test('frozen receipt retains pending error and never mutates after a late reply', async () => {
  let resolve!: (value: NativeAnchor) => void;
  const clock = new MeasurementClock(() => new Promise(done => { resolve = done; }), 'run');
  clock.start();
  const receipt = clock.receipt();
  resolve(native(1)); await Promise.resolve();
  expect(receipt.anchors[0]).toMatchObject({ native: null, receive_event: null, error: 'incomplete' });
  expect(clock.receipt()).toBe(receipt);
});

test('invalid identity is retained and stops capture without another request', async () => {
  vi.useFakeTimers();
  const request = vi.fn(async id => ({ ...native(id), run_id: 'wrong' }));
  const clock = new MeasurementClock(request, 'run');
  clock.start(); await vi.advanceTimersByTimeAsync(1000);
  expect(request).toHaveBeenCalledTimes(1);
  expect(clock.receipt()).toMatchObject({ stop_reason: 'anchor_error', anchors: [{ native: null, error: expect.stringContaining('identity') }] });
});

test('no-export measurements never request an unsupported native clock', () => {
  const request = vi.fn(async () => { throw new Error('unsupported'); });
  const recorder = new PerformanceRecorder('run', 8, { now: () => 1, timeOrigin: 123 }, () => 1);
  recorder.enableClockAlignment(request);
  const ordinal = recorder.begin('edit', { duringExport: false, duringImport: false })!;
  recorder.end(ordinal, 'canceled');
  expect(request).not.toHaveBeenCalled();
  expect(recorder.receipt().clock_alignment).toMatchObject({ stop_reason: 'finalized', anchors: [], sample_events: [{ ordinal, start_event: 1, end_event: 2 }] });
});

test('anchors never delay durable recording and arbitrary epoch values do not enter causal proof', async () => {
  vi.useFakeTimers();
  let now = 12;
  const frames: FrameRequestCallback[] = [];
  const recorder = new PerformanceRecorder('run', 8, { now: () => now, timeOrigin: -999999 }, cb => { frames.push(cb); return 1; });
  recorder.enableClockAlignment(() => new Promise(() => {}));
  recorder.startExportClock();
  const ordinal = recorder.begin('edit', { duringExport: true, duringImport: false })!;
  now = 32; recorder.durable(ordinal); recorder.present(ordinal, () => true);
  frames.shift()!(now); now = 42; frames.shift()!(now);
  expect(recorder.receipt().samples[0]).toMatchObject({ durable_us: 20_000, presentation_us: 30_000 });
});

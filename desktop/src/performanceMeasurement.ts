import { invoke } from '@tauri-apps/api/core';

export type MeasurementKind = 'cull' | 'edit' | 'browse';
export type MeasurementOutcome = 'complete' | 'backend_error' | 'presentation_mismatch' | 'superseded' | 'canceled' | 'incomplete';
export type MeasurementContext = { duringImport: boolean; duringExport: boolean };

export type MeasurementSample = {
  kind: MeasurementKind;
  ordinal: number;
  started_us: number;
  during_import: boolean;
  during_export: boolean;
  outcome: MeasurementOutcome;
  durable_us: number | null;
  presentation_us: number | null;
  search_response_us: number | null;
  first_thumbnail_us: number | null;
  visible_complete_us: number | null;
  page_rows: number | null;
  visible_count: number | null;
};

export type MeasurementReceipt = {
  readonly protocol: 1;
  readonly presentation_model: 'two_animation_frames';
  readonly context_model: 'last_observed_status_at_start';
  readonly run_id: string;
  readonly time_origin_ms: number;
  readonly overflowed: number;
  readonly thumbnail_diagnostics: ThumbnailDiagnostics;
  readonly samples: readonly MeasurementSample[];
  readonly scroll_capture?: ScrollCapture;
};

export type ScrollSnapshot = {
  identity: number;
  scroll_top_px: number;
  scroll_left_px: number;
  viewport_width_px: number;
  viewport_height_px: number;
  scroll_width_px: number;
  scroll_height_px: number;
};

export type ScrollCapture = {
  frame_model: 'request_animation_frame_timestamp_scroll_position';
  outcome: 'complete' | 'incomplete';
  reason: 'duration_elapsed' | 'manual_stop' | 'finalized' | 'unmounted' | 'target_changed' | 'frame_limit';
  started_us: number;
  ended_us: number;
  target_initial: ScrollSnapshot;
  target_final: ScrollSnapshot | null;
  frames: readonly (readonly [timestamp_us: number, scroll_top_px: number, scroll_left_px: number])[];
};

export type ThumbnailDiagnostics = {
  attempts: number;
  decode_completed: number;
  decode_failed: number;
  source_changed: number;
  disconnected: number;
  incomplete: number;
  zero_size: number;
  nonvisible: number;
  accepted: number;
  roster_tiles: number;
  pending_expected: number;
};

type ThumbnailPresentation = Exclude<keyof ThumbnailDiagnostics, 'attempts' | 'decode_completed' | 'decode_failed' | 'roster_tiles' | 'pending_expected'>;

type Active = {
  kind: MeasurementKind;
  ordinal: number;
  started: number;
  startedUs: number;
  duringImport: boolean;
  duringExport: boolean;
  durableUs: number | null;
  searchResponseUs: number | null;
  pageRows: number | null;
  expected?: Set<string>;
  presented?: Map<string, number>;
};

type Clock = { now: () => number; timeOrigin: number };
type Frame = (callback: FrameRequestCallback) => number;
type CancelFrame = (handle: number) => void;
type Timer = (callback: () => void, milliseconds: number) => ReturnType<typeof setTimeout>;
type CancelTimer = (handle: ReturnType<typeof setTimeout>) => void;
type ScrollTarget = () => HTMLElement | null;

const elapsedUs = (started: number, ended: number) => Math.max(0, Math.round((ended - started) * 1000));
const MAX_STARTED_US = 24 * 60 * 60 * 1_000_000;
const SCROLL_DURATION_MS = 5_000;
const MAX_SCROLL_FRAMES = 2_048;
const MAX_SCROLL_PX = 0xffff_ffff;
const MAX_SCROLL_EDGE_GAP_US = 100_000;

const boundedPixel = (value: number) => Math.min(MAX_SCROLL_PX, Math.max(0, Math.round(Number.isFinite(value) ? value : 0)));

export class PerformanceRecorder {
  private nextOrdinal = 0;
  private overflowed = 0;
  private frozen: MeasurementReceipt | null = null;
  private readonly active = new Map<number, Active>();
  private readonly samples: MeasurementSample[] = [];
  private scrollCapture: ScrollCapture | undefined;
  private activeScroll: {
    target: HTMLElement;
    started: number;
    initial: ScrollSnapshot;
    frames: [number, number, number][];
    frameHandle: number | null;
    timerHandle: ReturnType<typeof setTimeout>;
  } | null = null;
  private nextSurfaceIdentity = 0;
  private readonly surfaceIdentities = new WeakMap<HTMLElement, number>();
  private readonly thumbnailDiagnostics: ThumbnailDiagnostics = {
    attempts: 0, decode_completed: 0, decode_failed: 0, source_changed: 0,
    disconnected: 0, incomplete: 0, zero_size: 0, nonvisible: 0,
    accepted: 0, roster_tiles: 0, pending_expected: 0,
  };
  readonly runId: string;
  readonly maxSamples: number;
  private readonly clock: Clock;
  private readonly frame: Frame;
  private readonly cancelFrame: CancelFrame;
  private readonly timer: Timer;
  private readonly cancelTimer: CancelTimer;
  private readonly findScrollTarget: ScrollTarget;
  private readonly changed: () => void;

  constructor(
    runId: string,
    maxSamples: number,
    clock: Clock = performance,
    frame: Frame = requestAnimationFrame,
    changed: () => void = () => {},
    cancelFrame: CancelFrame = handle => cancelAnimationFrame(handle),
    timer: Timer = (callback, milliseconds) => setTimeout(callback, milliseconds),
    cancelTimer: CancelTimer = handle => clearTimeout(handle),
    findScrollTarget: ScrollTarget = () => document.querySelector<HTMLElement>('.photo-grid'),
  ) {
    this.runId = runId;
    this.maxSamples = maxSamples;
    this.clock = clock;
    this.frame = frame.bind(globalThis);
    this.cancelFrame = cancelFrame.bind(globalThis);
    this.timer = timer.bind(globalThis);
    this.cancelTimer = cancelTimer.bind(globalThis);
    this.findScrollTarget = findScrollTarget;
    this.changed = changed;
  }

  get count() { return this.samples.length; }
  get scrollState(): 'idle' | 'capturing' | 'complete' | 'incomplete' {
    return this.activeScroll ? 'capturing' : this.scrollCapture?.outcome ?? 'idle';
  }
  get scrollFrames() { return this.activeScroll?.frames.length ?? this.scrollCapture?.frames.length ?? 0; }

  startScroll(target: HTMLElement): boolean {
    if (this.frozen || this.activeScroll || this.scrollCapture || !target.isConnected) return false;
    if (target.scrollHeight <= target.clientHeight && target.scrollWidth <= target.clientWidth) return false;
    const started = this.clock.now();
    const active = {
      target, started, initial: this.scrollSnapshot(target), frames: [] as [number, number, number][], frameHandle: null as number | null,
      timerHandle: undefined as unknown as ReturnType<typeof setTimeout>,
    };
    this.activeScroll = active;
    const tick: FrameRequestCallback = timestamp => {
      if (this.activeScroll !== active) return;
      active.frameHandle = null;
      active.frames.push([Math.round(timestamp * 1000), boundedPixel(active.target.scrollTop), boundedPixel(active.target.scrollLeft)]);
      const current = this.findScrollTarget();
      if (!active.target.isConnected || !current) { this.stopScroll('unmounted'); return; }
      if (current !== active.target) { this.stopScroll('target_changed', current); return; }
      if (active.frames.length >= MAX_SCROLL_FRAMES) { this.stopScroll('frame_limit'); return; }
      active.frameHandle = this.frame(tick);
    };
    active.frameHandle = this.frame(tick);
    active.timerHandle = this.timer(() => this.stopScroll('duration_elapsed'), SCROLL_DURATION_MS);
    this.notify();
    return true;
  }

  stopScroll(reason: ScrollCapture['reason'] = 'manual_stop', finalTarget?: HTMLElement | null): boolean {
    const active = this.activeScroll;
    if (!active) return false;
    this.activeScroll = null;
    this.cancelTimer(active.timerHandle);
    if (active.frameHandle !== null) this.cancelFrame(active.frameHandle);
    const target = finalTarget === undefined ? this.findScrollTarget() : finalTarget;
    const sameTarget = target === active.target && active.target.isConnected;
    const ended = this.clock.now();
    const startedUs = Math.max(0, Math.round(active.started * 1000));
    const endedUs = Math.max(0, Math.round(ended * 1000));
    const firstFrameUs = active.frames[0]?.[0];
    const lastFrameUs = active.frames.at(-1)?.[0];
    this.scrollCapture = Object.freeze({
      frame_model: 'request_animation_frame_timestamp_scroll_position',
      outcome: reason === 'duration_elapsed' && sameTarget && ended - active.started >= SCROLL_DURATION_MS && ended - active.started <= 10_000 && firstFrameUs !== undefined && lastFrameUs !== undefined && Math.abs(firstFrameUs - startedUs) <= MAX_SCROLL_EDGE_GAP_US && lastFrameUs <= endedUs && endedUs - lastFrameUs <= MAX_SCROLL_EDGE_GAP_US ? 'complete' : 'incomplete',
      reason,
      started_us: startedUs,
      ended_us: endedUs,
      target_initial: active.initial,
      target_final: target ? this.scrollSnapshot(target) : null,
      frames: Object.freeze(active.frames.map(frame => Object.freeze(frame))),
    });
    this.notify();
    return true;
  }

  begin(kind: MeasurementKind, context: MeasurementContext): number | undefined {
    if (this.frozen) return undefined;
    if (this.active.size >= 32 || this.samples.length + this.active.size >= this.maxSamples) { this.overflowed += 1; this.notify(); return undefined; }
    const started = this.clock.now(), startedUs = Math.max(0, Math.round(started * 1000));
    if (startedUs > MAX_STARTED_US) { this.overflowed += 1; this.notify(); return undefined; }
    const ordinal = ++this.nextOrdinal;
    this.active.set(ordinal, { kind, ordinal, started, startedUs, duringImport: context.duringImport, duringExport: context.duringExport, durableUs: null, searchResponseUs: null, pageRows: null });
    return ordinal;
  }

  durable(ordinal: number) {
    const active = this.active.get(ordinal);
    if (active && active.kind !== 'browse' && active.durableUs === null) active.durableUs = elapsedUs(active.started, this.clock.now());
  }

  present(ordinal: number, verify: () => boolean) {
    this.afterTwoFrames(() => {
      const active = this.active.get(ordinal);
      if (!active || active.kind === 'browse') return;
      let presented = false;
      try { presented = verify(); } catch { /* A probe failure is a mismatch, never a product failure. */ }
      this.finish(active, presented ? 'complete' : 'presentation_mismatch', elapsedUs(active.started, this.clock.now()));
    });
  }

  end(ordinal: number, outcome: Exclude<MeasurementOutcome, 'complete' | 'presentation_mismatch' | 'incomplete'>) {
    const active = this.active.get(ordinal);
    // A later UI/navigation error cannot revoke an acknowledged durable write.
    if (outcome === 'backend_error' && active && active.durableUs !== null) return;
    if (active) this.finish(active, outcome, null);
  }

  searchResponse(ordinal: number, pageRows: number) {
    const active = this.active.get(ordinal);
    if (!active || active.kind !== 'browse') return;
    active.searchResponseUs = elapsedUs(active.started, this.clock.now());
    active.pageRows = pageRows;
  }

  visible(ordinal: number, tileIds: string[]) {
    const active = this.active.get(ordinal);
    if (!active || active.kind !== 'browse' || active.expected) return;
    const expected = new Set(tileIds);
    if (!expected.size) return;
    active.expected = expected;
    this.thumbnailDiagnostic('roster_tiles', expected.size);
    active.presented ??= new Map();
    this.finishBrowseIfReady(active);
  }

  thumbnailAttempt() { this.thumbnailDiagnostic('attempts'); }
  thumbnailDecodeCompleted() { this.thumbnailDiagnostic('decode_completed'); }
  thumbnailDecodeFailed() { this.thumbnailDiagnostic('decode_failed'); }

  thumbnailDecoded(ordinal: number, tileId: string, inspect: () => ThumbnailPresentation) {
    this.afterTwoFrames(() => {
      const active = this.active.get(ordinal);
      if (!active || active.kind !== 'browse') return;
      let outcome: ThumbnailPresentation = 'incomplete';
      try { outcome = inspect(); } catch { /* Leave the sample incomplete without affecting browsing. */ }
      if (outcome !== 'accepted') { this.thumbnailDiagnostic(outcome); return; }
      active.presented ??= new Map();
      if (!active.presented.has(tileId)) {
        active.presented.set(tileId, this.clock.now());
        this.thumbnailDiagnostic('accepted');
      }
      this.finishBrowseIfReady(active);
    });
  }

  receipt(): MeasurementReceipt {
    if (this.frozen) return this.frozen;
    this.stopScroll('finalized');
    for (const active of [...this.active.values()]) {
      if (active.kind === 'browse' && active.expected) {
        this.thumbnailDiagnostic('pending_expected', [...active.expected].filter(id => !active.presented?.has(id)).length);
      }
      this.finish(active, 'incomplete', null);
    }
    const samples = Object.freeze(this.samples.map(sample => Object.freeze({ ...sample })));
    this.frozen = Object.freeze({ protocol: 1, presentation_model: 'two_animation_frames', context_model: 'last_observed_status_at_start', run_id: this.runId, time_origin_ms: this.clock.timeOrigin, overflowed: this.overflowed, thumbnail_diagnostics: Object.freeze({ ...this.thumbnailDiagnostics }), samples, ...(this.scrollCapture ? { scroll_capture: this.scrollCapture } : {}) });
    return this.frozen;
  }

  // Two browser animation frames are a presentation opportunity. They do not
  // establish compositor delivery or physical scanout; those need native trace evidence.
  private afterTwoFrames(action: () => void) { this.frame(() => { this.frame(() => action()); }); }
  private notify() { try { this.changed(); } catch { /* Measurement status cannot affect product work. */ } }
  private surfaceIdentity(target: HTMLElement) {
    let identity = this.surfaceIdentities.get(target);
    if (identity === undefined) { identity = ++this.nextSurfaceIdentity; this.surfaceIdentities.set(target, identity); }
    return identity;
  }
  private scrollSnapshot(target: HTMLElement): ScrollSnapshot {
    return Object.freeze({
      identity: this.surfaceIdentity(target),
      scroll_top_px: boundedPixel(target.scrollTop), scroll_left_px: boundedPixel(target.scrollLeft),
      viewport_width_px: boundedPixel(target.clientWidth), viewport_height_px: boundedPixel(target.clientHeight),
      scroll_width_px: boundedPixel(target.scrollWidth), scroll_height_px: boundedPixel(target.scrollHeight),
    });
  }
  private thumbnailDiagnostic(kind: keyof ThumbnailDiagnostics, count = 1) {
    if (this.frozen) return;
    this.thumbnailDiagnostics[kind] = Math.min(0xffffffff, this.thumbnailDiagnostics[kind] + count);
  }

  private finishBrowseIfReady(active: Active) {
    if (!active.expected || !active.presented || ![...active.expected].every(id => active.presented!.has(id))) return;
    const times = [...active.expected].map(id => active.presented!.get(id)!);
    const first = Math.min(...times), complete = Math.max(...times);
    this.samples.push({
      kind: 'browse', ordinal: active.ordinal, started_us: active.startedUs, during_import: active.duringImport, outcome: 'complete',
      during_export: active.duringExport,
      durable_us: null, presentation_us: null, search_response_us: active.searchResponseUs,
      first_thumbnail_us: elapsedUs(active.started, first), visible_complete_us: elapsedUs(active.started, complete),
      page_rows: active.pageRows, visible_count: active.expected.size,
    });
    this.active.delete(active.ordinal); this.notify();
  }

  private finish(active: Active, outcome: MeasurementOutcome, presentationUs: number | null) {
    this.samples.push({
      kind: active.kind, ordinal: active.ordinal, started_us: active.startedUs, during_import: active.duringImport, outcome,
      during_export: active.duringExport,
      durable_us: active.durableUs, presentation_us: presentationUs,
      search_response_us: active.searchResponseUs, first_thumbnail_us: null, visible_complete_us: null,
      page_rows: active.pageRows, visible_count: active.expected?.size ?? null,
    });
    this.active.delete(active.ordinal); this.notify();
  }
}

type Config = { enabled: boolean; run_id: string | null; max_samples: number };
export type MeasurementStatus = { enabled: boolean; samples: number; scrollState: 'idle' | 'capturing' | 'complete' | 'incomplete'; scrollFrames: number; finalizing: boolean; finalized: boolean; receiptPath: string | null; error: string | null };

export class ReceiptFinalizer {
  private snapshot: MeasurementReceipt | null = null;
  private pending: Promise<string> | null = null;
  private completed: string | null = null;
  private readonly recorder: PerformanceRecorder;
  private readonly submit: (receipt: MeasurementReceipt) => Promise<string>;

  constructor(recorder: PerformanceRecorder, submit: (receipt: MeasurementReceipt) => Promise<string>) {
    this.recorder = recorder;
    this.submit = submit;
  }

  finish(): Promise<string> {
    if (this.completed !== null) return Promise.resolve(this.completed);
    if (this.pending) return this.pending;
    this.snapshot ??= this.recorder.receipt();
    const attempt = this.submit(this.snapshot).then(path => { this.completed = path; return path; });
    const pending = attempt.finally(() => { if (this.pending === pending) this.pending = null; });
    this.pending = pending;
    return this.pending;
  }
}

let recorder: PerformanceRecorder | null = null;
let finalizer: ReceiptFinalizer | null = null;
let initialization: Promise<void> | null = null;
let status: MeasurementStatus = { enabled: false, samples: 0, scrollState: 'idle', scrollFrames: 0, finalizing: false, finalized: false, receiptPath: null, error: null };
const listeners = new Set<(value: MeasurementStatus) => void>();
const publish = (next: Partial<MeasurementStatus>) => { status = { ...status, ...next }; listeners.forEach(listener => listener(status)); };

export function subscribeMeasurement(listener: (value: MeasurementStatus) => void) {
  listeners.add(listener); listener(status); return () => { listeners.delete(listener); };
}

export function initializeMeasurement() {
  initialization ??= invoke<Config>('catalog_measurement_config').then(config => {
    if (!config.enabled || !config.run_id) return;
    recorder = new PerformanceRecorder(config.run_id, Math.min(512, config.max_samples), performance, requestAnimationFrame, () => publish({ samples: recorder?.count ?? 0, scrollState: recorder?.scrollState ?? 'idle', scrollFrames: recorder?.scrollFrames ?? 0 }));
    finalizer = new ReceiptFinalizer(recorder, receipt => invoke<string>('catalog_measurement_finish', { receipt }));
    publish({ enabled: true });
  }).catch(error => publish({ error: error instanceof Error ? error.message : String(error) }));
  return initialization;
}

export const beginMeasurement = (kind: MeasurementKind, context: MeasurementContext) => recorder?.begin(kind, context);
export const measurementDurable = (ordinal: number | undefined) => { if (ordinal !== undefined) recorder?.durable(ordinal); };
export const measurementPresented = (ordinal: number | undefined, verify: () => boolean) => { if (ordinal !== undefined) recorder?.present(ordinal, verify); };
export const measurementEnded = (ordinal: number | undefined, outcome: 'backend_error' | 'superseded' | 'canceled') => { if (ordinal !== undefined) recorder?.end(ordinal, outcome); };
export const measurementSearchResponse = (ordinal: number | undefined, rows: number) => { if (ordinal !== undefined) recorder?.searchResponse(ordinal, rows); };
export const measurementVisible = (ordinal: number | undefined, ids: string[]) => { if (ordinal !== undefined) recorder?.visible(ordinal, ids); };
export function startScrollMeasurement() {
  const target = document.querySelector<HTMLElement>('.photo-grid');
  if (!target || !recorder?.startScroll(target)) {
    publish({ error: 'Scroll capture requires one mounted, scrollable photo grid.' });
    return false;
  }
  publish({ error: null });
  return true;
}
export const stopScrollMeasurement = () => recorder?.stopScroll('manual_stop') ?? false;

export function classifyThumbnailPresentation(image: Pick<HTMLImageElement, 'getAttribute' | 'isConnected' | 'complete' | 'naturalWidth' | 'naturalHeight' | 'getBoundingClientRect'>, expectedSource: string): ThumbnailPresentation {
  if (image.getAttribute('src') !== expectedSource) return 'source_changed';
  if (!image.isConnected) return 'disconnected';
  if (!image.complete) return 'incomplete';
  if (image.naturalWidth <= 0 || image.naturalHeight <= 0) return 'zero_size';
  const rect = image.getBoundingClientRect();
  if (!(rect.width > 0 && rect.height > 0 && rect.bottom > 0 && rect.right > 0 && rect.top < window.innerHeight && rect.left < window.innerWidth)) return 'nonvisible';
  return 'accepted';
}

export async function measurementThumbnail(ordinal: number | undefined, tileId: string, image: HTMLImageElement, expectedSource: string) {
  if (ordinal === undefined || !recorder) return;
  recorder.thumbnailAttempt();
  try { await image.decode(); }
  catch { recorder.thumbnailDecodeFailed(); return; }
  recorder.thumbnailDecodeCompleted();
  recorder.thumbnailDecoded(ordinal, tileId, () => classifyThumbnailPresentation(image, expectedSource));
}

export async function finalizeMeasurement() {
  if (!finalizer || status.finalized) return;
  publish({ finalizing: true, error: null });
  try {
    const receiptPath = await finalizer.finish();
    publish({ finalized: true, samples: recorder?.count ?? status.samples, receiptPath });
  } catch (error) { publish({ error: error instanceof Error ? error.message : String(error) }); }
  finally { publish({ finalizing: false }); }
}

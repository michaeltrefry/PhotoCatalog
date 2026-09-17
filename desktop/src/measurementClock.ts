export const ANCHOR_INTERVAL_MS = 100;
export const ANCHOR_DURATION_MS = 300_000;
export const IMPORT_ANCHOR_INTERVAL_MS = 200;
export const IMPORT_ANCHOR_DURATION_MS = 600_000;
export const MAX_ANCHORS = 3_002;
export const MAX_IMPORT_BINDINGS = 4;
export const MAX_IMPORT_TIMELINE = 1_202;

export type NativeAnchor = { run_id: string; anchor_id: number; session_id: string; native_pid: number; clock: 'macos_mach_absolute_ns'; monotonic_ns: string };
export type Anchor = { anchor_id: number; send_event: number; receive_event: number | null; native: NativeAnchor | null; error: string | null };
export type SampleEvents = { ordinal: number; start_event: number; durable_event?: number; end_event: number | null };
export type ImportPhase = 'discovering' | 'draining' | 'complete' | 'cancel_requested' | 'canceled' | 'failed';
export type ImportBinding = { key: number; id: string; source_blake3: string };
export type ImportTimelineInput = {
  phase: ImportPhase;
  imported: string;
  unchanged: string;
  failed: string;
  skipped: string;
  metadata_updated: string;
  metadata_warnings: string;
  awaiting_resources: string;
  pending_previews: number;
};
export type ImportTimeline = ImportTimelineInput & { request_event: number; event: number; binding: number };
export type ImportEvidence = { bindings: readonly ImportBinding[]; timeline: readonly ImportTimeline[]; overflowed: number };
export type ClockAlignment = {
  model: 'causal_native_brackets_v1';
  profile?: 'import_v1';
  interval_ms: number;
  duration_ms: number;
  anchors: readonly Anchor[];
  sample_events: readonly SampleEvents[];
  import_evidence?: ImportEvidence;
  stop_reason: string;
};

type Profile = 'export_v1' | 'import_v1';
const sameImportStatus = (left: ImportTimeline, right: ImportTimelineInput) => left.phase === right.phase
  && left.imported === right.imported && left.unchanged === right.unchanged && left.failed === right.failed
  && left.skipped === right.skipped && left.metadata_updated === right.metadata_updated
  && left.metadata_warnings === right.metadata_warnings && left.awaiting_resources === right.awaiting_resources
  && left.pending_previews === right.pending_previews;
const profileTiming = (profile: Profile) => profile === 'import_v1'
  ? { interval: IMPORT_ANCHOR_INTERVAL_MS, duration: IMPORT_ANCHOR_DURATION_MS }
  : { interval: ANCHOR_INTERVAL_MS, duration: ANCHOR_DURATION_MS };

/** Event order supplies causal bounds; no wall-clock or equal-rate conversion. */
export class MeasurementClock {
  private event = 0;
  private started = false;
  private stopped = false;
  private frozen: ClockAlignment | null = null;
  private pending = false;
  private timer: ReturnType<typeof setTimeout> | undefined;
  private deadlineTimer: ReturnType<typeof setTimeout> | undefined;
  private readonly anchors: Anchor[] = [];
  private readonly samples: SampleEvents[] = [];
  private readonly importBindings: ImportBinding[] = [];
  private readonly importTimeline: ImportTimeline[] = [];
  private readonly importRequests = new Set<number>();
  private importOverflowed = 0;
  private profile: Profile | null = null;
  private stopReason = 'not_started';
  private readonly request: (id: number) => Promise<NativeAnchor>;
  private readonly runId: string;
  constructor(request: (id: number) => Promise<NativeAnchor>, runId: string) { this.request = request; this.runId = runId; }

  begin(ordinal: number) { this.samples.push({ ordinal, start_event: ++this.event, end_event: null }); }
  durable(ordinal: number, importBound = false) {
    if (this.profile !== 'import_v1' || !importBound) return;
    const sample = this.samples.find(value => value.ordinal === ordinal);
    if (sample && sample.durable_event === undefined) sample.durable_event = ++this.event;
  }
  end(ordinal: number) {
    const sample = this.samples.find(value => value.ordinal === ordinal);
    if (sample && sample.end_event === null) sample.end_event = ++this.event;
  }
  start(profile: Profile = 'export_v1'): boolean {
    if (this.started) return this.profile === profile;
    if (this.stopped) return false;
    this.started = true;
    this.profile = profile;
    this.stopReason = 'capturing';
    const timing = profileTiming(profile);
    this.deadlineTimer = setTimeout(() => this.stop('duration_elapsed'), timing.duration);
    void this.probe();
    return true;
  }
  beginImportRequest(): number | undefined {
    if (this.frozen || !this.start('import_v1') || this.importRequests.size >= 8) return;
    const event = ++this.event;
    this.importRequests.add(event);
    return event;
  }
  discardImportRequest(event: number | undefined) { if (event !== undefined) this.importRequests.delete(event); }
  observeImport(requestEvent: number, id: string, sourceBlake3: string, status: ImportTimelineInput): boolean {
    if (this.frozen || this.profile !== 'import_v1' || !this.importRequests.delete(requestEvent)) return false;
    let binding = this.importBindings.find(value => value.id === id);
    if (binding && binding.source_blake3 !== sourceBlake3) {
      this.importOverflowed += 1;
      return false;
    }
    if (!binding) {
      if (this.importBindings.length >= MAX_IMPORT_BINDINGS) {
        this.importOverflowed += 1;
        return false;
      }
      binding = { key: this.importBindings.length + 1, id, source_blake3: sourceBlake3 };
      this.importBindings.push(binding);
    }
    let previous: ImportTimeline | undefined;
    for (let index = this.importTimeline.length - 1; index >= 0; index -= 1) {
      if (this.importTimeline[index].binding === binding.key) { previous = this.importTimeline[index]; break; }
    }
    if (previous && sameImportStatus(previous, status)) return true;
    if (this.importTimeline.length >= MAX_IMPORT_TIMELINE) {
      this.importOverflowed += 1;
      return false;
    }
    this.importTimeline.push({ request_event: requestEvent, event: ++this.event, binding: binding.key, ...status });
    return true;
  }
  private async probe() {
    if (this.stopped || this.pending) return;
    if (this.anchors.length >= MAX_ANCHORS) { this.stop('anchor_limit'); return; }
    this.pending = true;
    const anchor: Anchor = { anchor_id: this.anchors.length + 1, send_event: ++this.event, receive_event: null, native: null, error: null };
    this.anchors.push(anchor);
    try {
      const native = await this.request(anchor.anchor_id);
      if (this.frozen) return;
      anchor.receive_event = ++this.event;
      if (native.run_id !== this.runId || native.anchor_id !== anchor.anchor_id || native.clock !== 'macos_mach_absolute_ns'
        || !Number.isInteger(native.native_pid) || native.native_pid <= 0 || !/^[0-9]+$/.test(native.monotonic_ns)
        || native.monotonic_ns.length > 20 || !native.session_id) throw new Error('Native clock anchor identity is invalid');
      anchor.native = native;
    } catch (error) {
      if (this.frozen) return;
      anchor.receive_event ??= ++this.event;
      anchor.error = String(error).slice(0, 256);
      this.stop('anchor_error');
    } finally {
      this.pending = false;
      if (!this.stopped) this.timer = setTimeout(() => void this.probe(), profileTiming(this.profile ?? 'export_v1').interval);
    }
  }
  private stop(reason: string) {
    if (this.stopped) return;
    this.stopped = true;
    this.stopReason = reason;
    clearTimeout(this.timer); clearTimeout(this.deadlineTimer);
  }
  receipt(): ClockAlignment {
    if (this.frozen) return this.frozen;
    this.stop('finalized');
    const importEvidence = this.profile === 'import_v1' ? Object.freeze({
      bindings: Object.freeze(this.importBindings.map(value => Object.freeze({ ...value }))),
      timeline: Object.freeze(this.importTimeline.map(value => Object.freeze({ ...value }))),
      overflowed: this.importOverflowed,
    }) : undefined;
    this.frozen = Object.freeze({
      model: 'causal_native_brackets_v1',
      ...(this.profile === 'import_v1' ? { profile: 'import_v1' as const } : {}),
      interval_ms: profileTiming(this.profile ?? 'export_v1').interval,
      duration_ms: profileTiming(this.profile ?? 'export_v1').duration,
      anchors: Object.freeze(this.anchors.map(anchor => Object.freeze({ ...anchor, native: anchor.native ? Object.freeze({ ...anchor.native }) : null,
        error: anchor.receive_event === null ? 'incomplete' : anchor.error }))),
      sample_events: Object.freeze(this.samples.map(sample => Object.freeze({ ...sample }))),
      ...(importEvidence ? { import_evidence: importEvidence } : {}),
      stop_reason: this.stopReason,
    });
    return this.frozen;
  }
}

export const ANCHOR_INTERVAL_MS = 100;
export const ANCHOR_DURATION_MS = 300_000;
export const MAX_ANCHORS = 3_002;
export type NativeAnchor = { run_id: string; anchor_id: number; session_id: string; native_pid: number; clock: 'macos_mach_absolute_ns'; monotonic_ns: string };
export type Anchor = { anchor_id: number; send_event: number; receive_event: number | null; native: NativeAnchor | null; error: string | null };
export type SampleEvents = { ordinal: number; start_event: number; end_event: number | null };
export type ClockAlignment = { model: 'causal_native_brackets_v1'; interval_ms: number; duration_ms: number; anchors: readonly Anchor[]; sample_events: readonly SampleEvents[]; stop_reason: string };

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
  private stopReason = 'not_started';
  private readonly request: (id: number) => Promise<NativeAnchor>;
  private readonly runId: string;
  constructor(request: (id: number) => Promise<NativeAnchor>, runId: string) { this.request = request; this.runId = runId; }

  begin(ordinal: number) { this.samples.push({ ordinal, start_event: ++this.event, end_event: null }); }
  end(ordinal: number) {
    const sample = this.samples.find(value => value.ordinal === ordinal);
    if (sample && sample.end_event === null) sample.end_event = ++this.event;
  }
  start() {
    if (this.started || this.stopped) return;
    this.started = true;
    this.stopReason = 'capturing';
    this.deadlineTimer = setTimeout(() => this.stop('duration_elapsed'), ANCHOR_DURATION_MS);
    void this.probe();
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
      if (!this.stopped) this.timer = setTimeout(() => void this.probe(), ANCHOR_INTERVAL_MS);
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
    this.frozen = Object.freeze({ model: 'causal_native_brackets_v1', interval_ms: ANCHOR_INTERVAL_MS, duration_ms: ANCHOR_DURATION_MS,
      anchors: Object.freeze(this.anchors.map(anchor => Object.freeze({ ...anchor, native: anchor.native ? Object.freeze({ ...anchor.native }) : null,
        error: anchor.receive_event === null ? 'incomplete' : anchor.error }))),
      sample_events: Object.freeze(this.samples.map(sample => Object.freeze({ ...sample }))), stop_reason: this.stopReason });
    return this.frozen;
  }
}

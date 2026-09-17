export type CleanupDisposition = 'busy' | 'retired' | 'failure';

type Send = (catalog: string, viewport: string, generation: string) => Promise<void>;
type Schedule = (run: () => void, delayMs: number) => void;
type Report = (error: unknown) => void;

type ReservationState = {
  generation: bigint;
  encoded: string;
  started: boolean;
  settled: boolean;
  released: boolean;
  resolved: boolean;
  promise: Promise<boolean>;
  resolve: (ready: boolean) => void;
  reject: (error: unknown) => void;
};

type Entry = {
  catalog: string;
  viewport: string;
  current: ReservationState;
  waiting?: ReservationState;
  failure?: unknown;
  retries: number;
};

export type PreviewCleanupLease = {
  ready: Promise<boolean>;
  begin: () => boolean;
  settled: () => void;
  release: () => void;
};

export class PreviewCleanupCapacityError extends Error {
  constructor() {
    super('Waiting for earlier previews to finish…');
    this.name = 'PreviewCleanupCapacityError';
  }
}

const keyOf = (catalog: string, viewport: string) => JSON.stringify([catalog, viewport]);

/**
 * Reserves bounded cleanup state before a preview can be admitted. Generations
 * for one viewport are serialized: a stale release is confirmed before a newer
 * preview starts, while never-started remounts coalesce to the highest value.
 */
export class PreviewCleanupCoordinator {
  private readonly entries = new Map<string, Entry>();
  private readonly send: Send;
  private readonly classify: (error: unknown) => CleanupDisposition;
  private readonly schedule: Schedule;
  private readonly report: Report;
  private readonly capacity: number;
  private activeCatalog: string | undefined;
  private running: ReservationState | null = null;
  private retryScheduled = false;

  constructor(
    send: Send,
    classify: (error: unknown) => CleanupDisposition,
    schedule: Schedule = (run, delayMs) => { setTimeout(run, delayMs); },
    report: Report = error => { console.error('Preview cleanup failed', error); },
    capacity = 128,
  ) {
    this.send = send;
    this.classify = classify;
    this.schedule = schedule;
    this.report = report;
    this.capacity = capacity;
  }

  reserve(catalog: string, viewport: string, generation: string): PreviewCleanupLease {
    let parsed: bigint;
    try { parsed = BigInt(generation); }
    catch { return this.rejected(new Error('Preview generation is invalid.')); }
    if (parsed <= 0n) return this.rejected(new Error('Preview generation is invalid.'));
    // App owns one catalog token at a time. A token change proves every old
    // viewport stale and frees even a retained unexpected-failure entry.
    if (this.activeCatalog !== catalog) {
      if (this.activeCatalog !== undefined) this.retire(this.activeCatalog);
      this.activeCatalog = catalog;
    }
    const key = keyOf(catalog, viewport);
    let entry = this.entries.get(key);
    if (entry?.failure !== undefined) return this.rejected(entry.failure);
    if (!entry) {
      if (this.entries.size >= this.capacity) return this.rejected(new PreviewCleanupCapacityError());
      const state = this.state(parsed, generation);
      entry = { catalog, viewport, current: state, retries: 0 };
      this.entries.set(key, entry);
      this.resolve(state, true);
      return this.lease(key, state);
    }
    const newest = entry.waiting?.generation ?? entry.current.generation;
    if (parsed <= newest) return this.rejected(new Error('Preview generation did not advance.'));
    if (entry.waiting) this.resolve(entry.waiting, false);
    const state = this.state(parsed, generation);
    entry.waiting = state;
    return this.lease(key, state);
  }

  get retained(): number { return this.entries.size; }

  private state(generation: bigint, encoded: string): ReservationState {
    let resolve!: (ready: boolean) => void;
    let reject!: (error: unknown) => void;
    const promise = new Promise<boolean>((accept, decline) => { resolve = accept; reject = decline; });
    return { generation, encoded, started: false, settled: false, released: false, resolved: false, promise, resolve, reject };
  }

  private lease(key: string, state: ReservationState): PreviewCleanupLease {
    return {
      ready: state.promise,
      begin: () => this.begin(key, state),
      settled: () => this.settled(key, state),
      release: () => this.release(key, state),
    };
  }

  private rejected(error: unknown): PreviewCleanupLease {
    return { ready: Promise.reject(error), begin: () => false, settled: () => {}, release: () => {} };
  }

  private resolve(state: ReservationState, ready: boolean) {
    if (state.resolved) return;
    state.resolved = true;
    state.resolve(ready);
  }

  private reject(state: ReservationState, error: unknown) {
    if (state.resolved) return;
    state.resolved = true;
    state.reject(error);
  }

  private begin(key: string, state: ReservationState): boolean {
    const entry = this.entries.get(key);
    if (!entry || entry.current !== state || state.started || state.released) return false;
    state.started = true;
    return true;
  }

  private settled(key: string, state: ReservationState) {
    const entry = this.entries.get(key);
    if (!entry || entry.current !== state || !state.started) return;
    state.settled = true;
    if (state.released) this.kick();
  }

  private release(key: string, state: ReservationState) {
    const entry = this.entries.get(key);
    if (!entry || state.released) return;
    state.released = true;
    if (entry.waiting === state) {
      entry.waiting = undefined;
      this.resolve(state, false);
      return;
    }
    if (entry.current !== state) return;
    if (!state.started) this.complete(key, entry, state);
    else if (state.settled) this.kick();
  }

  private kick() {
    if (this.running || this.retryScheduled) return;
    for (const [key, entry] of this.entries) {
      const state = entry.current;
      if (entry.failure === undefined && state.started && state.settled && state.released) {
        this.running = state;
        void this.send(entry.catalog, entry.viewport, state.encoded).then(
          () => {
            this.running = null;
            if (this.entries.get(key) !== entry || entry.current !== state) { this.kick(); return; }
            entry.retries = 0;
            this.complete(key, entry, state);
          },
          error => {
            this.running = null;
            if (this.entries.get(key) !== entry || entry.current !== state) { this.kick(); return; }
            const disposition = this.classify(error);
            if (disposition === 'busy') {
              entry.retries += 1;
              // Let other ready releases use the next retry turn instead of
              // allowing one pressured viewport to pin the front of the map.
              this.entries.delete(key);
              this.entries.set(key, entry);
              const delay = Math.min(500, 25 * 2 ** Math.min(4, entry.retries - 1));
              this.retryScheduled = true;
              this.schedule(() => { this.retryScheduled = false; this.kick(); }, delay);
            } else if (disposition === 'retired') {
              this.retire(entry.catalog);
              this.kick();
            } else {
              entry.failure = error;
              if (entry.waiting) { this.reject(entry.waiting, error); entry.waiting = undefined; }
              this.report(error);
              this.kick();
            }
          },
        );
        return;
      }
    }
  }

  private complete(key: string, entry: Entry, state: ReservationState) {
    if (this.entries.get(key) !== entry || entry.current !== state) { this.kick(); return; }
    const next = entry.waiting;
    if (next) {
      entry.current = next;
      entry.waiting = undefined;
      entry.retries = 0;
      this.resolve(next, true);
      if (next.released) this.complete(key, entry, next);
    } else this.entries.delete(key);
    this.kick();
  }

  private retire(catalog: string) {
    for (const [key, entry] of this.entries) {
      if (entry.catalog !== catalog) continue;
      if (entry.waiting) this.resolve(entry.waiting, false);
      this.entries.delete(key);
    }
    if (this.activeCatalog === catalog) this.activeCatalog = undefined;
  }
}

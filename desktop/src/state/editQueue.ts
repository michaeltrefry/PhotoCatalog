import type { Variant } from '../bridge';
import type { Recipe } from '../recipe';

export type EditSnapshot = { variant: Variant; recipe: Recipe; state: 'saved' | 'pending' | 'saving' | 'error'; error?: string };
export type EditMeasurementEvent = { ordinal: number; outcome: 'durable' | 'superseded' | 'backend_error'; revision?: string };
export const EDIT_COALESCE_MS = 16;
export const EDIT_BUSY_RETRY_BASE_MS = 16;
export const EDIT_BUSY_RETRY_MAX_MS = 128;
export const EDIT_BUSY_RETRY_LIMIT = 18;

const busyRetryDelay = (retry: number) => Math.min(EDIT_BUSY_RETRY_BASE_MS * 2 ** (retry - 1), EDIT_BUSY_RETRY_MAX_MS);
const wait = (milliseconds: number) => new Promise<void>(resolve => setTimeout(resolve, milliseconds));

// Serialize CAS writes for one variant while retaining slider changes made during
// an in-flight save. Navigation awaits flush instead of dropping a dirty recipe.
export class EditQueue {
  private snapshot: EditSnapshot;
  private listeners = new Set<(value: EditSnapshot) => void>();
  private version = 0;
  private saved = 0;
  private active: Promise<void> | undefined;
  private timer: ReturnType<typeof setTimeout> | undefined;
  private measured: { version: number; ordinal: number } | undefined;
  private readonly save: (value: Variant, recipe: Recipe) => Promise<Variant>;
  private readonly measurement?: (event: EditMeasurementEvent) => void;
  // This predicate must only recognize a definitive rejection before mutation.
  // Conflicts and transport failures keep the existing terminal behavior.
  private readonly retryablePreMutation: (error: unknown) => boolean;
  constructor(
    variant: Variant,
    save: (value: Variant, recipe: Recipe) => Promise<Variant>,
    measurement?: (event: EditMeasurementEvent) => void,
    retryablePreMutation: (error: unknown) => boolean = () => false,
  ) {
    this.save = save;
    this.measurement = measurement;
    this.retryablePreMutation = retryablePreMutation;
    this.snapshot = { variant, recipe: variant.recipe, state: 'saved' };
  }
  get value() { return this.snapshot; }
  subscribe(listener: (value: EditSnapshot) => void) { this.listeners.add(listener); listener(this.snapshot); return () => { this.listeners.delete(listener); }; }
  private publish(value: EditSnapshot) { this.snapshot = value; this.listeners.forEach(listener => listener(value)); }
  private measure(event: EditMeasurementEvent) { try { this.measurement?.(event); } catch { /* Opt-in measurement cannot change edit durability. */ } }
  change(recipe: Recipe, measurementOrdinal?: number) {
    if (this.measured) this.measure({ ordinal: this.measured.ordinal, outcome: 'superseded' });
    ++this.version;
    this.measured = measurementOrdinal === undefined ? undefined : { version: this.version, ordinal: measurementOrdinal };
    this.publish({ ...this.snapshot, recipe, state: this.active ? 'saving' : 'pending', error: undefined });
    clearTimeout(this.timer);
    this.timer = setTimeout(() => { void this.flush().catch(() => {}); }, EDIT_COALESCE_MS);
  }
  async flush(): Promise<void> {
    clearTimeout(this.timer);
    if (this.active) return this.active;
    if (this.saved === this.version) return;
    this.active = this.write();
    try { await this.active; } finally { this.active = undefined; }
  }
  private async write() {
    let busyRetries = 0;
    while (this.saved !== this.version) {
      const version = this.version;
      const recipe = this.snapshot.recipe;
      this.publish({ ...this.snapshot, state: 'saving', error: undefined });
      try {
        const variant = await this.save(this.snapshot.variant, recipe);
        this.saved = version;
        this.publish({ variant, recipe: version === this.version ? variant.recipe : this.snapshot.recipe, state: version === this.version ? 'saved' : 'pending' });
        if (this.measured?.version === version && version === this.version) {
          this.measure({ ordinal: this.measured.ordinal, outcome: 'durable', revision: variant.revision });
          this.measured = undefined;
        }
      } catch (error) {
        if (this.retryablePreMutation(error) && busyRetries < EDIT_BUSY_RETRY_LIMIT) {
          busyRetries += 1;
          this.publish({ ...this.snapshot, state: 'saving', error: undefined });
          await wait(busyRetryDelay(busyRetries));
          continue;
        }
        clearTimeout(this.timer);
        this.publish({ ...this.snapshot, state: 'error', error: error instanceof Error ? error.message : String(error) });
        if (this.measured) this.measure({ ordinal: this.measured.ordinal, outcome: 'backend_error' });
        this.measured = undefined;
        throw error;
      }
    }
  }
}

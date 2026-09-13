import type { Variant } from '../bridge';
import type { Recipe } from '../recipe';

export type EditSnapshot = { variant: Variant; recipe: Recipe; state: 'saved' | 'pending' | 'saving' | 'error'; error?: string };

// Serialize CAS writes for one variant while retaining slider changes made during
// an in-flight save. Navigation awaits flush instead of dropping a dirty recipe.
export class EditQueue {
  private snapshot: EditSnapshot;
  private listeners = new Set<(value: EditSnapshot) => void>();
  private version = 0;
  private saved = 0;
  private active: Promise<void> | undefined;
  private timer: ReturnType<typeof setTimeout> | undefined;
  private readonly save: (value: Variant, recipe: Recipe) => Promise<Variant>;
  constructor(variant: Variant, save: (value: Variant, recipe: Recipe) => Promise<Variant>) {
    this.save = save;
    this.snapshot = { variant, recipe: variant.recipe, state: 'saved' };
  }
  get value() { return this.snapshot; }
  subscribe(listener: (value: EditSnapshot) => void) { this.listeners.add(listener); listener(this.snapshot); return () => { this.listeners.delete(listener); }; }
  private publish(value: EditSnapshot) { this.snapshot = value; this.listeners.forEach(listener => listener(value)); }
  change(recipe: Recipe) {
    ++this.version;
    this.publish({ ...this.snapshot, recipe, state: this.active ? 'saving' : 'pending', error: undefined });
    clearTimeout(this.timer);
    this.timer = setTimeout(() => { void this.flush().catch(() => {}); }, 150);
  }
  async flush(): Promise<void> {
    clearTimeout(this.timer);
    if (this.active) return this.active;
    if (this.saved === this.version) return;
    this.active = this.write();
    try { await this.active; } finally { this.active = undefined; }
  }
  private async write() {
    while (this.saved !== this.version) {
      const version = this.version;
      const recipe = this.snapshot.recipe;
      this.publish({ ...this.snapshot, state: 'saving', error: undefined });
      try {
        const variant = await this.save(this.snapshot.variant, recipe);
        this.saved = version;
        this.publish({ variant, recipe: version === this.version ? variant.recipe : this.snapshot.recipe, state: version === this.version ? 'saved' : 'pending' });
      } catch (error) {
        clearTimeout(this.timer);
        this.publish({ ...this.snapshot, state: 'error', error: error instanceof Error ? error.message : String(error) });
        throw error;
      }
    }
  }
}

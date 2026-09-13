/** Synchronous admission spans flush, native reply, and selection replacement. */
export class ActionGate {
  private active = false;
  private waiters: (() => void)[] = [];
  get locked() { return this.active; }
  async run(action: () => Promise<void>): Promise<boolean> {
    if (this.active) return false;
    this.active = true;
    try { await action(); return true; }
    finally { this.active = false; this.waiters.splice(0).forEach(resolve => resolve()); }
  }
  async afterCurrent(action: () => Promise<void>): Promise<void> {
    while (this.active) await new Promise<void>(resolve => this.waiters.push(resolve));
    if (!await this.run(action)) await this.afterCurrent(action);
  }
}

import { describe, expect, it } from 'vitest';
import { OrganizationRunner } from './organizationRunner';
import { ActionGate } from './actionGate';
import type { Job } from '../organization';
const ready: Job = { id: 'durable-job', operation: { operation: 'rating', value: 4 }, state: 'ready', pending: '2', applied: '0', failed: '0', skipped: '0' };
function deferred<T>() { let resolve!: (value: T) => void; const promise = new Promise<T>(done => { resolve = done; }); return { promise, resolve }; }
describe('durable organization runner', () => {
  it('stops after an acknowledged in-flight item without pretending cancellation rolls it back', async () => {
    const runner = new OrganizationRunner(); const pending = deferred<Job>(); const seen: Job[] = []; let calls = 0;
    const running = runner.run(ready, async () => { calls++; return pending.promise; }, job => seen.push(job));
    runner.stop(); const committed = { ...ready, state: 'running', applied: '1', pending: '1' }; pending.resolve(committed); await running;
    expect(calls).toBe(1); expect(seen).toEqual([committed]);
    await runner.run(committed, async () => { calls++; return { ...committed, state: 'complete', pending: '0', applied: '2' }; }, job => seen.push(job));
    expect(calls).toBe(2); expect(seen.at(-1)?.state).toBe('complete');
  });
  it('stops on a conflict and requires a separately reviewed resume', async () => {
    const runner = new OrganizationRunner(); let calls = 0;
    await runner.run(ready, async () => { calls++; return { ...ready, state: 'paused', failed: '1', pending: '1' }; }, () => {});
    expect(calls).toBe(1);
  });
  it('flushes edits before each mutation and serializes cancel behind the in-flight commit', async () => {
    const gate = new ActionGate(); const pending = deferred<void>(); const events: string[] = [];
    const runner = new OrganizationRunner();
    const run = runner.run(ready, async () => { await gate.afterCurrent(async () => { events.push('flush'); events.push('step'); await pending.promise; events.push('commit'); }); return { ...ready, applied: '1', pending: '1', state: 'running' }; }, () => {});
    runner.stop(); const cancel = gate.afterCurrent(async () => { events.push('cancel'); });
    expect(events).toEqual(['flush','step']); pending.resolve(); await Promise.all([run, cancel]); expect(events).toEqual(['flush','step','commit','cancel']);
  });
  it('rejects a second runner admission and surfaces step errors for reopening', async () => {
    const runner = new OrganizationRunner(); const pending = deferred<Job>();
    const first = runner.run(ready, () => pending.promise, () => {});
    await expect(runner.run(ready, () => pending.promise, () => {})).rejects.toThrow('already running'); pending.resolve({ ...ready, state: 'complete' }); await first;
    await expect(runner.run(ready, async () => { throw new Error('catalog closed'); }, () => {})).rejects.toThrow('catalog closed');
  });
});

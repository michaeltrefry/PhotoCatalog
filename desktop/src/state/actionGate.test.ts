import { expect, test, vi } from 'vitest';
import { ActionGate } from './actionGate';

function deferred() { let resolve!: () => void; const promise = new Promise<void>(done => { resolve = done; }); return { promise, resolve }; }

test('selection owns admission from pending flush through delayed native reply', async () => {
  const gate = new ActionGate();
  const flush = deferred(); const reply = deferred();
  let selected = 'A'; let queue = 'A'; let draft = 1;
  const transition = gate.run(async () => { await flush.promise; await reply.promise; selected = 'B'; queue = 'B'; });
  const edit = () => { if (!gate.locked) draft = 2; };
  edit();
  const competing = vi.fn(async () => { selected = 'C'; });
  expect(await gate.run(competing)).toBe(false);
  flush.resolve(); await Promise.resolve();
  edit();
  expect([selected, queue, draft]).toEqual(['A', 'A', 1]);
  reply.resolve(); await transition;
  expect([selected, queue, draft]).toEqual(['B', 'B', 1]);
  expect(competing).not.toHaveBeenCalled();
  edit(); expect(draft).toBe(2);
});

test('quit waits for the current undo or cull reply before flushing and closing', async () => {
  const gate = new ActionGate(); const reply = deferred(); const closing = deferred();
  const events: string[] = [];
  const undo = gate.run(async () => { await reply.promise; events.push('attach undo to A'); });
  const quit = gate.afterCurrent(async () => { events.push('flush A'); await closing.promise; events.push('quit'); });
  expect(events).toEqual([]);
  reply.resolve(); await undo; await Promise.resolve();
  expect(events).toEqual(['attach undo to A', 'flush A']);
  expect(await gate.run(async () => { events.push('late selection'); })).toBe(false);
  closing.resolve(); await quit;
  expect(events).toEqual(['attach undo to A', 'flush A', 'quit']);
});

test('failed flush releases admission without executing replacement or discarding retry', async () => {
  const gate = new ActionGate(); const replace = vi.fn();
  await expect(gate.run(async () => { throw new Error('Save failed'); replace(); })).rejects.toThrow('Save failed');
  expect(replace).not.toHaveBeenCalled();
  expect(gate.locked).toBe(false);
  expect(await gate.run(async () => { replace(); })).toBe(true);
});

import { beforeEach, expect, test, vi } from 'vitest';

const harness = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/api/core', () => ({ invoke: harness.invoke, isTauri: () => true }));

import { command } from './bridge';

type Deferred<T> = { promise: Promise<T>; resolve: (value: T) => void; reject: (error: Error) => void };
function deferred<T>(): Deferred<T> {
  let resolve!: (value: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((accept, decline) => { resolve = accept; reject = decline; });
  return { promise, resolve, reject };
}

const reply = { status: 'ok', value: { kind: 'status', data: { phase: 'ready' } } };

beforeEach(() => { harness.invoke.mockReset(); });

test('late abort awaits cancellation before settling its fulfilled catalog request', async () => {
  const main = deferred<typeof reply>(), cancel = deferred<void>();
  harness.invoke.mockImplementation((name: string) => {
    if (name === 'catalog_command') return main.promise;
    if (name === 'catalog_cancel_operation') return cancel.promise;
    if (name === 'catalog_settle_cancellation') return Promise.resolve();
    throw new Error(`unexpected command ${name}`);
  });
  const abort = new AbortController();
  const pending = command({ command: 'status' }, 'status', abort.signal);
  main.resolve(reply);
  abort.abort();
  await Promise.resolve();
  expect(harness.invoke.mock.calls.map(([name]) => name)).toEqual(['catalog_command', 'catalog_cancel_operation']);
  cancel.resolve();
  await expect(pending).rejects.toMatchObject({ name: 'AbortError' });
  expect(harness.invoke.mock.calls.map(([name]) => name)).toEqual(['catalog_command', 'catalog_cancel_operation', 'catalog_settle_cancellation']);
  const operation = harness.invoke.mock.calls[0][1].operation;
  expect(harness.invoke.mock.calls[1][1]).toEqual({ operation });
  expect(harness.invoke.mock.calls[2][1]).toEqual({ operation });
});

test('early cancellation is not settled until the main request fulfills', async () => {
  const main = deferred<typeof reply>(), cancel = deferred<void>();
  harness.invoke.mockImplementation((name: string) => {
    if (name === 'catalog_command') return main.promise;
    if (name === 'catalog_cancel_operation') return cancel.promise;
    if (name === 'catalog_settle_cancellation') return Promise.resolve();
    throw new Error(`unexpected command ${name}`);
  });
  const abort = new AbortController();
  const pending = command({ command: 'status' }, 'status', abort.signal);
  abort.abort();
  cancel.resolve();
  await Promise.resolve();
  expect(harness.invoke.mock.calls.map(([name]) => name)).toEqual(['catalog_command', 'catalog_cancel_operation']);
  main.resolve(reply);
  await expect(pending).rejects.toMatchObject({ name: 'AbortError' });
  expect(harness.invoke.mock.calls.map(([name]) => name)).toEqual(['catalog_command', 'catalog_cancel_operation', 'catalog_settle_cancellation']);
});

test('rejected main invocation preserves its error and does not settle ambiguous cancellation', async () => {
  const main = deferred<typeof reply>();
  harness.invoke.mockImplementation((name: string) => {
    if (name === 'catalog_command') return main.promise;
    if (name === 'catalog_cancel_operation') return Promise.resolve();
    throw new Error(`unexpected command ${name}`);
  });
  const abort = new AbortController();
  const pending = command({ command: 'status' }, 'status', abort.signal);
  abort.abort();
  const failure = new Error('catalog transport failed');
  main.reject(failure);
  await expect(pending).rejects.toBe(failure);
  expect(harness.invoke.mock.calls.map(([name]) => name)).toEqual(['catalog_command', 'catalog_cancel_operation']);
});

test('fulfilled main request still settles when cancellation and cleanup IPC reject', async () => {
  const main = deferred<typeof reply>();
  harness.invoke.mockImplementation((name: string) => {
    if (name === 'catalog_command') return main.promise;
    if (name === 'catalog_cancel_operation') return Promise.reject(new Error('cancel reply lost'));
    if (name === 'catalog_settle_cancellation') return Promise.reject(new Error('settle reply lost'));
    throw new Error(`unexpected command ${name}`);
  });
  const abort = new AbortController();
  const pending = command({ command: 'status' }, 'status', abort.signal);
  abort.abort();
  main.resolve(reply);
  await expect(pending).rejects.toMatchObject({ name: 'AbortError' });
  expect(harness.invoke.mock.calls.map(([name]) => name)).toEqual(['catalog_command', 'catalog_cancel_operation', 'catalog_settle_cancellation']);
});

test('ordinary fulfilled command sends no cancellation cleanup IPC', async () => {
  harness.invoke.mockResolvedValue(reply);
  await expect(command({ command: 'status' }, 'status', new AbortController().signal)).resolves.toEqual({ phase: 'ready' });
  expect(harness.invoke.mock.calls.map(([name]) => name)).toEqual(['catalog_command']);
});

import { expect, it, vi } from 'vitest';
const { command } = vi.hoisted(() => ({ command: vi.fn() }));
vi.mock('./bridge', () => ({ command }));
import { enteredReference, pathLabel, referenceLabel, relink, relinkTerminal, type RelinkOperation, type RuleCursor } from './relink';

it('preserves non-UTF8 original identity while keeping display separate', () => {
  const reference = { Native: { encoding: 'UnixBytes' as const, units: [47, 255, 65] } };
  expect(referenceLabel(reference)).toBe('Path with non-UTF8 bytes: 2f ff 41');
  expect(reference.Native.units).toEqual([47, 255, 65]);
  expect(pathLabel({ encoding: 'WindowsWide', units: [67, 58, 92, 0xd800] })).toBe('C:\\\ud800');
});

it('requires an explicit absolute old-folder path and retains UTF16 code units', () => {
  expect(enteredReference('/Volumes/旧/Raw', 'unix')).toEqual({ LegacyUnix: [...new TextEncoder().encode('/Volumes/旧/Raw')] });
  expect(enteredReference('C:\\\ud800', 'windows')).toEqual({ LegacyWindows: [67, 58, 92, 0xd800] });
  for (const value of ['', 'relative/path', '/bad\0path']) expect(() => enteredReference(value, 'unix')).toThrow();
  for (const value of ['C:relative', '\\only', '/Volumes/Raw']) expect(() => enteredReference(value, 'windows')).toThrow();
});

it('never treats a terminal label with an owned write hold as finished', () => {
  const operation = { phase: 'complete', write_hold: true } as RelinkOperation;
  expect(relinkTerminal(operation)).toBe(false);
  expect(relinkTerminal({ ...operation, write_hold: false })).toBe(true);
  expect(relinkTerminal({ ...operation, phase: 'cancel_requested', write_hold: false })).toBe(false);
});

it('transports a sparse rule continuation and large revisions without numeric conversion', async () => {
  const cursor: RuleCursor = { plan: 'saved', revision: '9007199254740993', stage: 'excluded_source', position: '9007199254740995', source: '9007199254740997', entity: 'source' };
  command.mockResolvedValueOnce({ kind: 'rules', value: { rows: [], scanned: '20', next: cursor } });
  const page = await relink('catalog-session', { command: 'rules', args: { plan: 'saved', revision: cursor.revision, after: null, limit: '20' } }, 'rules');
  command.mockResolvedValueOnce({ kind: 'rules', value: { rows: [], scanned: '1', next: null } });
  const request = { command: 'rules' as const, args: { plan: 'saved', revision: cursor.revision, after: page.next, limit: '20' } };
  await relink('catalog-session', request, 'rules');
  expect(command).toHaveBeenLastCalledWith({ command: 'relink', args: { catalog: 'catalog-session', request } }, 'relink', undefined);
});

it('keeps volume observation unit-shaped and rejects an unexpected response', async () => {
  command.mockResolvedValueOnce({ kind: 'operation', value: null });
  await relink('catalog-session', { command: 'mounts' }, 'operation');
  expect(command).toHaveBeenLastCalledWith({ command: 'relink', args: { catalog: 'catalog-session', request: { command: 'mounts' } } }, 'relink', undefined);
  command.mockResolvedValueOnce({ kind: 'plan', value: {} });
  await expect(relink('catalog-session', { command: 'mounts' }, 'operation')).rejects.toThrow('Unexpected relink response');
});

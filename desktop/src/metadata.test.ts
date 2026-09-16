import { describe, expect, it, vi } from 'vitest';
const { command } = vi.hoisted(() => ({ command: vi.fn() }));
vi.mock('./bridge', () => ({ command }));
import { chunkHex, metadata, readableValue, settingsPath } from './metadata';
import type { ImageIdentity } from './organization';

describe('retained metadata fidelity and selected variant transport', () => {
  it('builds named, numeric and namespaced settings paths without integer rounding', () => {
    expect(settingsPath([{ kind: 'name', name: 'Develop"Settings' }, { kind: 'index', index: '9007199254740993' }, { kind: 'xml', namespace: 'urn:crs', name: 'Settings', ordinal: '18446744073709551615' }])).toBe('[{"kind":"name","value":"Develop\\"Settings"},{"kind":"index","value":9007199254740993},{"kind":"xml","value":{"namespace":"urn:crs","name":"Settings","ordinal":18446744073709551615}}]');
    for (const index of ['-1', '01', '1.0', '18446744073709551616']) expect(() => settingsPath([{ kind: 'index', index }])).toThrow();
  });
  it('keeps large numeric evidence lexical while decoding only text strings', () => {
    for (const text of ['9007199254740993', '{"id":9223372036854775807}', '[9007199254740993]', 'not json']) expect(readableValue(text)).toBe(text);
    expect(readableValue('"Caption\\nsecond line"')).toBe('Caption\nsecond line');
  });
  it('displays every byte with exact large offsets including non-UTF8 packets', () => {
    const bytes = [255, 254, 65, 0, 61, 216, 0, 222, ...Array.from({ length: 10 }, (_, i) => i)];
    const hex = chunkHex(bytes, '9007199254740993');
    expect(hex.split('\n')[0]).toContain('20000000000001  ff fe 41 00 3d d8 00 de');
    expect(hex.split('\n')[1]).toContain('20000000000011  08 09');
  });
  it('replays an empty-page continuation without rounding identity or interpreting the cursor', async () => {
    const identity: ImageIdentity = { image_id: 'logical-copy', key: { asset_id: 'same-raw', variant_id: 'copy' }, metadata_revision: '9007199254740993', pixel_generation: '4', shared_source_epoch: '5', physical_generation: '6' };
    const cursor = '{"opaque":9007199254740997}';
    command.mockResolvedValueOnce({ kind: 'candidates', value: { rows: [], next: cursor, scanned: '20' } });
    const page = await metadata('session', { command: 'candidates', args: { identity, field: 'title', after: null, limit: 20 } }, 'candidates');
    command.mockResolvedValueOnce({ kind: 'candidates', value: { rows: [], next: null, scanned: '1' } });
    await metadata('session', { command: 'candidates', args: { identity, field: 'title', after: page.next, limit: 20 } }, 'candidates');
    expect(command).toHaveBeenLastCalledWith({ command: 'metadata', args: { catalog: 'session', request: { command: 'candidates', args: { identity, field: 'title', after: cursor, limit: 20 } } } }, 'metadata', undefined);
  });
  it('sends the reviewed copy revision and rejects unexpected responses', async () => {
    const request = { command: 'resolve' as const, args: { key: { asset_id: 'same-raw', variant_id: 'copy' }, expected_revision: '9007199254740993', field: 'title', model: '9007199254740997' } };
    command.mockResolvedValueOnce({ kind: 'changed', value: { revision: '9007199254740994' } });
    expect(await metadata('session', request, 'changed')).toEqual({ revision: '9007199254740994' });
    expect(command).toHaveBeenLastCalledWith({ command: 'metadata', args: { catalog: 'session', request } }, 'metadata', undefined);
    command.mockResolvedValueOnce({ kind: 'identity', value: {} });
    await expect(metadata('session', request, 'changed')).rejects.toThrow('Unexpected metadata response');
  });
});

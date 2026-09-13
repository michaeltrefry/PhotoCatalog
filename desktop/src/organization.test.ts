import { describe, expect, it, vi } from 'vitest';
const { command } = vi.hoisted(() => ({ command: vi.fn() }));
vi.mock('./bridge', () => ({ command }));
import { nonnegativeDecimal, organize, selectionPage, jobName, type MembershipCursor } from './organization';
import type { GridImage } from './bridge';
const photo = (variant: string): GridImage => ({ image_id: `image-${variant}`, key: { asset_id: 'same original', variant_id: variant }, sequence: '9007199254740993', metadata_revision: '9223372036854775806', metadata_pending: false, state: 'ready', filename: 'photo.raw', rating: null, flag: 'unflagged', label: '', conflicts: [] });
describe('organization review identity and wire contract', () => {
  it('preserves variants and exact reviewed revisions while copying the chosen page', () => {
    const original = photo('original'); const copy = photo('copy');
    const selected = selectionPage([original, copy]); original.key.variant_id = 'changed';
    expect(selected).toEqual([{ key: { asset_id: 'same original', variant_id: 'original' }, expected_revision: '9223372036854775806' }, { key: { asset_id: 'same original', variant_id: 'copy' }, expected_revision: '9223372036854775806' }]);
    expect(() => selectionPage([])).toThrow(); expect(() => selectionPage([copy, copy])).toThrow(); expect(() => selectionPage(Array.from({ length: 101 }, (_, n) => photo(String(n))))).toThrow();
  });
  it('keeps collection and revision bound continuation even on an empty member page', async () => {
    const next: MembershipCursor = { collection: 'collection-id', revision: '9007199254740993', ordered: false, position: '0', sequence: '9007199254740995' };
    command.mockResolvedValueOnce({ kind: 'members', value: { rows: [], next, scanned: '20' } });
    const page = await organize('catalog-token', { command: 'members', args: { collection: next.collection, after: null, limit: 20 } }, 'members');
    expect(page.next).toEqual(next);
    command.mockResolvedValueOnce({ kind: 'members', value: { rows: [], next: null, scanned: '1' } });
    await organize('catalog-token', { command: 'members', args: { collection: next.collection, after: page.next, limit: 20 } }, 'members');
    expect(command).toHaveBeenLastCalledWith({ command: 'organization', args: { catalog: 'catalog-token', request: { command: 'members', args: { collection: next.collection, after: next, limit: 20 } } } }, 'organization', undefined);
  });
  it('handles unit acknowledgments and rejects wrong response kinds', async () => {
    command.mockResolvedValueOnce({ kind: 'acknowledged' });
    await expect(organize('catalog', { command: 'rename_collection', args: { id: 'real chosen collection', expected_revision: '9007199254740993', name: 'Travel' } }, 'acknowledged')).resolves.toBeUndefined();
    command.mockResolvedValueOnce({ kind: 'placement', value: {} });
    await expect(organize('catalog', { command: 'jobs', args: { after: '', limit: 20 } }, 'jobs')).rejects.toThrow('Unexpected organization response');
  });
  it('resolves named collection targets by one exact lookup and keeps deletion explicit', async () => {
    const collection = { id: 'retained-id', name: 'Japan 2026', revision: '9007199254740993', provenance_json: '{}' };
    command.mockResolvedValueOnce({ kind: 'collection', value: collection });
    expect(await organize('catalog', { command: 'collection', args: { id: collection.id } }, 'collection')).toEqual(collection);
    const job = { id: 'job', operation: { operation: 'add_collection' as const, collection: collection.id }, state: 'ready', pending: '2', applied: '0', failed: '0', skipped: '0' };
    expect(jobName(job, collection)).toBe('Add to collection “Japan 2026”');
    expect(jobName(job, null)).toContain('deleted or unavailable');
    command.mockResolvedValueOnce({ kind: 'collection', value: null });
    expect(await organize('catalog', { command: 'collection', args: { id: collection.id } }, 'collection')).toBeNull();
  });
  it('does not coerce collection positions through JavaScript numbers', () => {
    expect(nonnegativeDecimal('9007199254740993')).toBe('9007199254740993'); expect(nonnegativeDecimal('9223372036854775807')).toBe('9223372036854775807');
    for (const value of ['01', '-1', '1.5', '1e3', '9223372036854775808', '']) expect(() => nonnegativeDecimal(value)).toThrow();
  });
});

import { beforeEach, expect, test, vi } from 'vitest';
const harness = vi.hoisted(() => ({ effects: [] as (() => (() => void))[], command: vi.fn(), setValue: vi.fn(), previewBlob: vi.fn(), logPreviewDiagnostic: vi.fn(), diagnostics: false }));
vi.mock('react', () => ({
  useEffect: (effect: () => (() => void)) => harness.effects.push(effect),
  useRef: () => ({ current: 0n }),
  useState: () => [{ loading: true }, harness.setValue],
}));
vi.mock('../bridge', () => {
  class CatalogError extends Error {
    readonly code: string;
    constructor(code: string, message: string) { super(message); this.code = code; }
  }
  return { CatalogError, command: harness.command, errorText: String, imageKey: JSON.stringify, isBusyError: (error: unknown) => error instanceof CatalogError && error.code === 'busy', previewBlob: harness.previewBlob, logPreviewDiagnostic: harness.logPreviewDiagnostic };
});
vi.mock('../performanceMeasurement', () => ({ measurementDiagnosticsEnabled: () => harness.diagnostics }));
import { usePreview } from './usePreview';

beforeEach(() => { harness.effects.length = 0; harness.command.mockReset(); harness.setValue.mockReset(); harness.previewBlob.mockReset(); harness.logPreviewDiagnostic.mockReset(); harness.diagnostics = false; });
for (const outcome of ['success', 'failure'] as const) {
  test(`unmount before native preview admission releases again after ${outcome}`, async () => {
    let finish!: (value: unknown) => void; let fail!: (error: Error) => void;
    const pending = new Promise((resolve, reject) => { finish = resolve; fail = reject; });
    harness.command.mockImplementation(request => request.command === 'preview' ? pending : Promise.resolve({}));
    usePreview('catalog', { asset_id: 'asset', variant_id: 'master' }, 'tile', false, '1');
    const cleanup = harness.effects[0]();
    await Promise.resolve(); await Promise.resolve();
    const [request, , signal] = harness.command.mock.calls[0];
    cleanup();
    expect(signal.aborted).toBe(true);
    const releases = () => harness.command.mock.calls.filter(([value]) => value.command === 'release_viewport');
    expect(releases()).toHaveLength(0);
    if (outcome === 'success') finish({ ticket: 'late-ticket' }); else fail(new Error('Canceled before admission'));
    // Drain the then/catch/finally chain used by the actual hook.
    await pending.catch(() => {}); for (let index = 0; index < 6; index += 1) await Promise.resolve();
    expect(releases()).toHaveLength(1);
    for (const [release] of releases()) expect(release.args).toEqual({ catalog: 'catalog', viewport: 'tile', generation: request.args.generation });
    expect(harness.command.mock.calls.some(([value]) => value.command === 'preview_status')).toBe(false);
  });
}

test('opt-in remount emits one bounded phase record after delivery readback', async () => {
  harness.diagnostics = true;
  const native = {
    route: 'retained', expected_key_digest: 'a'.repeat(64), selected_key_digest: 'a'.repeat(64), current_key_matches_selected: true,
    retained_read: { outcome: 'ready', queue_ms: 1, owner_read_ms: 2, catalog_identity_ms: 0.1, store_read_checksum_ms: 0.5, header_decode_ms: 1.4, total_ms: 2, decoded_hits: 0, decoded_misses: 1 },
    original_render_ms: null, ready_ms: 3,
    delivery: { ready_for_transfer_ms: 1, retained_read: { outcome: 'ready', queue_ms: 0.5, owner_read_ms: 1, catalog_identity_ms: 0.1, store_read_checksum_ms: 0.4, header_decode_ms: 0.5, total_ms: 1, decoded_hits: 1, decoded_misses: 0 }, transfer_ms: 1, total_ms: 2 },
  };
  const ready = { ticket: '00000000-0000-4000-8000-000000000001', key: { asset_id: 'asset', variant_id: 'master' }, revision: '1', recipe_digest: 'recipe', viewport: 'tile', generation: '2', state: 'ready', message: null, diagnostic: native };
  let generation = '';
  harness.command.mockImplementation(request => {
    if (request.command === 'preview') generation = request.args.generation;
    return Promise.resolve({ ...ready, generation });
  });
  harness.previewBlob.mockResolvedValue(new Blob([new Uint8Array([1])]));
  harness.logPreviewDiagnostic.mockResolvedValue(true);
  const create = vi.spyOn(URL, 'createObjectURL').mockReturnValue('blob:diagnostic');
  usePreview('catalog', ready.key, 'tile', false, '1');
  const cleanup = harness.effects[0]();
  for (let index = 0; index < 12; index += 1) await Promise.resolve();
  expect(harness.command.mock.calls[0][0].args.diagnostics).toBe(true);
  expect(harness.previewBlob).toHaveBeenCalledWith('catalog', ready.ticket);
  expect(harness.logPreviewDiagnostic).toHaveBeenCalledTimes(1);
  expect(harness.logPreviewDiagnostic.mock.calls[0][0]).toMatchObject({ ticket: ready.ticket, polls: 1, native });
  cleanup(); for (let index = 0; index < 4; index += 1) await Promise.resolve();
  create.mockRestore();
});

test('ordinary successful delivery performs no diagnostic readback or log', async () => {
  let generation = '';
  harness.command.mockImplementation(request => {
    if (request.command === 'preview') {
      generation = request.args.generation;
      return Promise.resolve({ ticket: 'ordinary-ticket' });
    }
    return Promise.resolve({
      ticket: 'ordinary-ticket', key: { asset_id: 'asset', variant_id: 'master' }, revision: '1', recipe_digest: 'recipe',
      viewport: 'tile', generation, state: 'ready', message: null,
    });
  });
  harness.previewBlob.mockResolvedValue(new Blob([new Uint8Array([1])]));
  const create = vi.spyOn(URL, 'createObjectURL').mockReturnValue('blob:ordinary');
  usePreview('catalog', { asset_id: 'asset', variant_id: 'master' }, 'tile', false, '1');
  const cleanup = harness.effects[0]();
  for (let index = 0; index < 12; index += 1) await Promise.resolve();
  expect(harness.command.mock.calls[0][0].args.diagnostics).toBe(false);
  expect(harness.command.mock.calls.filter(([value]) => value.command === 'preview_status')).toHaveLength(1);
  expect(harness.logPreviewDiagnostic).not.toHaveBeenCalled();
  cleanup(); for (let index = 0; index < 4; index += 1) await Promise.resolve();
  create.mockRestore();
});

test('unmount during delayed diagnostic readback emits no stale update or log and revokes URL', async () => {
  harness.diagnostics = true;
  let finishReadback!: (value: unknown) => void;
  const delayedReadback = new Promise(resolve => { finishReadback = resolve; });
  let generation = '';
  let statuses = 0;
  const ready = {
    ticket: '00000000-0000-4000-8000-000000000002', key: { asset_id: 'asset', variant_id: 'master' }, revision: '1', recipe_digest: 'recipe',
    viewport: 'tile', generation: '', state: 'ready', message: null,
  };
  harness.command.mockImplementation(request => {
    if (request.command === 'preview') {
      generation = request.args.generation;
      return Promise.resolve({ ticket: ready.ticket });
    }
    if (request.command === 'preview_status' && statuses++ === 0) return Promise.resolve({ ...ready, generation });
    if (request.command === 'preview_status') return delayedReadback;
    return Promise.resolve({});
  });
  harness.previewBlob.mockResolvedValue(new Blob([new Uint8Array([1])]));
  const create = vi.spyOn(URL, 'createObjectURL').mockReturnValue('blob:delayed');
  const revoke = vi.spyOn(URL, 'revokeObjectURL').mockImplementation(() => {});
  usePreview('catalog', ready.key, 'tile', false, '1');
  const cleanup = harness.effects[0]();
  for (let index = 0; index < 12; index += 1) await Promise.resolve();
  expect(create).toHaveBeenCalledTimes(1);
  expect(harness.command.mock.calls.filter(([value]) => value.command === 'preview_status')).toHaveLength(2);
  const updates = harness.setValue.mock.calls.length;
  cleanup();
  expect(revoke).toHaveBeenCalledWith('blob:delayed');
  finishReadback({ ...ready, generation, diagnostic: {} });
  for (let index = 0; index < 4; index += 1) await Promise.resolve();
  expect(harness.setValue).toHaveBeenCalledTimes(updates);
  expect(harness.logPreviewDiagnostic).not.toHaveBeenCalled();
  create.mockRestore();
  revoke.mockRestore();
});

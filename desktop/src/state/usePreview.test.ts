import { beforeEach, expect, test, vi } from 'vitest';
const harness = vi.hoisted(() => ({ effects: [] as (() => (() => void))[], command: vi.fn(), setValue: vi.fn() }));
vi.mock('react', () => ({
  useEffect: (effect: () => (() => void)) => harness.effects.push(effect),
  useRef: () => ({ current: 0n }),
  useState: () => [{ loading: true }, harness.setValue],
}));
vi.mock('../bridge', () => ({ command: harness.command, errorText: String, imageKey: JSON.stringify, previewBlob: vi.fn() }));
import { usePreview } from './usePreview';

beforeEach(() => { harness.effects.length = 0; harness.command.mockReset(); harness.setValue.mockReset(); });
for (const outcome of ['success', 'failure'] as const) {
  test(`unmount before native preview admission releases again after ${outcome}`, async () => {
    let finish!: (value: unknown) => void; let fail!: (error: Error) => void;
    const pending = new Promise((resolve, reject) => { finish = resolve; fail = reject; });
    harness.command.mockImplementation(request => request.command === 'preview' ? pending : Promise.resolve({}));
    usePreview('catalog', { asset_id: 'asset', variant_id: 'master' }, 'tile', false, '1');
    const cleanup = harness.effects[0]();
    const [request, , signal] = harness.command.mock.calls[0];
    cleanup();
    expect(signal.aborted).toBe(true);
    const releases = () => harness.command.mock.calls.filter(([value]) => value.command === 'release_viewport');
    expect(releases()).toHaveLength(1);
    if (outcome === 'success') finish({ ticket: 'late-ticket' }); else fail(new Error('Canceled before admission'));
    // Drain the then/catch/finally chain used by the actual hook.
    await pending.catch(() => {}); await Promise.resolve(); await Promise.resolve();
    expect(releases()).toHaveLength(2);
    for (const [release] of releases()) expect(release.args).toEqual({ catalog: 'catalog', viewport: 'tile', generation: request.args.generation });
    expect(harness.command.mock.calls.some(([value]) => value.command === 'preview_status')).toBe(false);
  });
}

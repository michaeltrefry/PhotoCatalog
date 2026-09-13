import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { ReactElement } from 'react';
import type { CatalogStatus } from './bridge';
const state = vi.hoisted(() => ({ calls: 0, status: { phase: 'ready', catalog: 'same-catalog', jobs_held: false, pending_commands: 0, active_previews: 0, cancel_requested: false, message: null } as CatalogStatus }));
vi.mock('react', async importOriginal => ({
  ...await importOriginal<typeof import('react')>(),
  useState: (initial: unknown) => [state.calls++ === 0 ? state.status : typeof initial === 'function' ? initial() : initial, () => {}],
  useEffect: () => {}, useRef: (current: unknown) => ({ current }), useCallback: (callback: unknown) => callback,
}));
import { App } from './App';
import { OrganizationPanel } from './components/OrganizationPanel';
import { ImportPanel } from './components/ImportPanel';
import { CatalogActivity } from './components/CatalogActivity';
function find(node: unknown, type: unknown): ReactElement<Record<string, unknown>> | undefined {
  if (Array.isArray(node)) { for (const child of node) { const match = find(child, type); if (match) return match; } return; }
  if (!node || typeof node !== 'object' || !('type' in node)) return;
  const element = node as ReactElement<Record<string, unknown>>;
  return element.type === type ? element : find(element.props.children, type);
}
function render(phase: CatalogStatus['phase'], catalog: string | null = 'same-catalog') { state.calls = 0; state.status = { ...state.status, phase, catalog }; return App(); }
beforeEach(() => { state.calls = 0; });
describe('organization ownership in the actual App tree', () => {
  it('retains the same keyed controller through ready → indexing → ready and keeps preparation feedback', () => {
    const ready = find(render('ready'), OrganizationPanel)!;
    const indexingTree = render('indexing'); const indexing = find(indexingTree, OrganizationPanel)!;
    const resumed = find(render('ready'), OrganizationPanel)!;
    expect(ready).toBeDefined(); expect(indexing).toBeDefined(); expect(resumed).toBeDefined();
    expect([ready.key, indexing.key, resumed.key]).toEqual(['same-catalog', 'same-catalog', 'same-catalog']);
    expect([find(render('ready'), ImportPanel)?.key, find(indexingTree, ImportPanel)?.key, find(render('ready'), ImportPanel)?.key]).toEqual(['same-catalog', 'same-catalog', 'same-catalog']);
    expect(indexing.props.phase).toBe('indexing'); expect(find(indexingTree, CatalogActivity)).toBeDefined();
  });
  it('removes ownership on close and replaces its key for a different catalog session', () => {
    expect(find(render('closed', null), OrganizationPanel)).toBeUndefined();
    expect(find(render('closed', null), ImportPanel)).toBeUndefined();
    expect(find(render('ready', 'replacement-catalog'), ImportPanel)?.key).toBe('replacement-catalog');
    expect(find(render('ready', 'replacement-catalog'), OrganizationPanel)?.key).toBe('replacement-catalog');
  });
});

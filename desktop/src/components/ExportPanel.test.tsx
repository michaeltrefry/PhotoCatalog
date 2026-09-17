import { isValidElement, type ReactElement, type ReactNode } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it, vi } from 'vitest';
import { ImportedPathPreparationControls, requiresImportedPathPreparation, type ImportedPathPreparation } from './ExportPanel';

type Button = ReactElement<{ children?: ReactNode; disabled?: boolean; onClick?: () => void }>;
const text = (node: ReactNode): string => Array.isArray(node)
  ? node.map(text).join('')
  : isValidElement(node)
    ? text((node.props as { children?: ReactNode }).children)
    : typeof node === 'string' || typeof node === 'number' ? String(node) : '';
function button(node: ReactNode, label: string): Button | undefined {
  if (Array.isArray(node)) { for (const child of node) { const found = button(child, label); if (found) return found; } return; }
  if (!isValidElement(node)) return;
  if (node.type === 'button' && text((node.props as { children?: ReactNode }).children) === label) return node as Button;
  return button((node.props as { children?: ReactNode }).children, label);
}
const controls = (state: ImportedPathPreparation | null, uncertain: boolean, prepare = vi.fn(), confirm = vi.fn()) => ImportedPathPreparationControls({
  state, uncertain, prepare, confirm, progress: '', rows: '256', locked: uncertain, busy: false, setRows: vi.fn(),
});

describe('imported export path preparation guidance', () => {
  it('recognizes the authoritative native prerequisite without replacing its diagnostic', () => {
    const diagnostic = 'action failed: export alias index has pending path projections; run bounded reconciliation first';
    expect(requiresImportedPathPreparation(diagnostic)).toBe(true);
    expect(requiresImportedPathPreparation('catalog originals have undeclared native paths')).toBe(false);
    const confirm = vi.fn(), prepare = vi.fn();
    const tree = controls({ phase: 'required', diagnostic }, true, prepare, confirm);
    const html = renderToStaticMarkup(tree);
    expect(html).toContain('Imported photo locations need preparation');
    expect(html).toContain(diagnostic);
    expect(html).toContain('does not replay the rejected append');
    expect(prepare).not.toHaveBeenCalled(); expect(confirm).not.toHaveBeenCalled();
    expect(button(tree, 'Prepare one path step')).toBeUndefined();
    button(tree, 'Confirm saved job state')?.props.onClick?.();
    expect(confirm).toHaveBeenCalledTimes(1); expect(prepare).not.toHaveBeenCalled();
  });

  it('issues one bounded preparation step only after an explicit click', () => {
    const prepare = vi.fn();
    const tree = controls({ phase: 'pending' }, false, prepare);
    const html = renderToStaticMarkup(tree);
    expect(html).toContain('More imported photo locations need preparation');
    expect(prepare).not.toHaveBeenCalled();
    const action = button(tree, 'Prepare one path step'); expect(action?.props.disabled).toBe(false);
    action?.props.onClick?.(); expect(prepare).toHaveBeenCalledTimes(1);
  });

  it('makes the prerequisite discoverable and gives distinct terminal next steps', () => {
    expect(renderToStaticMarkup(controls(null, false))).toContain('prepare before first append');
    const complete = renderToStaticMarkup(controls({ phase: 'complete' }, false));
    expect(complete).toContain('Retry Append this reviewed output explicitly');
    expect(complete).toContain('will not retry it automatically');
    const unbound = renderToStaticMarkup(controls({ phase: 'unbound' }, false));
    expect(unbound).toContain('use Locate originals');
    expect(unbound).not.toContain('Prepare one path step');
  });
});

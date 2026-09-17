import { describe, expect, it } from 'vitest';
import type { Operation } from '../photoExport';
import { exportHoldsRecipeEdits, recipeControlsHeld } from './editAdmission';

const operation = (values: Partial<Operation>): Operation => ({
  id: 'operation', kind: 'run', phase: 'running', job: null, sequence: null,
  stage: 'rendering', stream_bytes: null, processed: '0', write_hold: true,
  result: null, error: null, ...values,
});

describe('recipe edit admission during export', () => {
  it('keeps recipe inputs enabled as a Run yields to foreground previews', () => {
    expect(exportHoldsRecipeEdits({ ready: true, admitting: false, operation: operation({}) })).toBe(false);
    expect(exportHoldsRecipeEdits({ ready: true, admitting: false, operation: operation({ write_hold: false }) })).toBe(false);
    expect(exportHoldsRecipeEdits({ ready: true, admitting: false, operation: operation({ stage: 'yielding' }) })).toBe(false);
  });

  it('retains unknown, admission, recovery, cancellation and drain holds', () => {
    expect(exportHoldsRecipeEdits({ ready: false, admitting: false, operation: null })).toBe(true);
    expect(exportHoldsRecipeEdits({ ready: true, admitting: true, operation: operation({}) })).toBe(true);
    expect(exportHoldsRecipeEdits({ ready: true, admitting: false, operation: operation({ kind: 'recover', stage: 'recovering' }) })).toBe(true);
    expect(exportHoldsRecipeEdits({ ready: true, admitting: false, operation: operation({ phase: 'cancel_requested' }) })).toBe(true);
    expect(exportHoldsRecipeEdits({ ready: true, admitting: false, operation: operation({ stage: 'draining' }) })).toBe(true);
    expect(exportHoldsRecipeEdits({ ready: true, admitting: false, operation: operation({ stage: 'yielding', phase: 'cancel_requested' }) })).toBe(true);
    expect(exportHoldsRecipeEdits({ ready: true, admitting: false, operation: operation({ stage: 'yielding', kind: 'recover' }) })).toBe(true);
    expect(exportHoldsRecipeEdits({ ready: true, admitting: false, operation: operation({ stage: 'waiting_for_previews', phase: 'waiting_for_previews' }) })).toBe(true);
  });

  it('does not let the Run exception override any independent hard hold', () => {
    const base = { transitioning: false, storage: false, directExport: false, migration: false, copy: false, export: false };
    expect(recipeControlsHeld(base)).toBe(false);
    for (const hold of ['transitioning', 'storage', 'directExport', 'migration', 'copy', 'export'] as const) {
      expect(recipeControlsHeld({ ...base, [hold]: true }), hold).toBe(true);
    }
    expect(recipeControlsHeld({ ...base, storage: true, migration: true, export: false })).toBe(true);
  });
});

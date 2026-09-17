import { describe, expect, it } from 'vitest';
import type { Operation } from '../photoExport';
import { exportHoldsRecipeEdits, recipeControlsHeld } from './editAdmission';

const operation = (values: Partial<Operation>): Operation => ({
  id: 'operation', kind: 'run', phase: 'running', job: null, sequence: null,
  stage: 'rendering', stream_bytes: null, processed: '0', write_hold: true,
  result: null, error: null, ...values,
});

describe('recipe edit admission during export', () => {
  it('keeps inputs stable across the entire ordinary Run lifecycle', () => {
    const stages: Record<Operation['stage'], boolean> = {
      opening: false, planning: false, hashing: false, alias: false,
      waiting_for_previews: false, rendering: false, accepting: false,
      intent_committed: false, captured: false, capture_verified: false,
      linked: false, finalizing: false, installed_verified: false,
      recovering: false, restoring: false, yielding: false,
      draining: true, finished: true,
    };
    for (const [stage, held] of Object.entries(stages)) {
      expect(exportHoldsRecipeEdits({ ready: true, admitting: false,
        operation: operation({ stage: stage as Operation['stage'] }) }), stage).toBe(held);
    }
    expect(exportHoldsRecipeEdits({ ready: true, admitting: false, operation: operation({ write_hold: false }) })).toBe(false);
  });

  it('does not interpret progress labels as authority for other phases or operations', () => {
    const phases: Operation['phase'][] = ['waiting_for_previews', 'paused', 'cancel_requested', 'complete', 'canceled', 'failed'];
    for (const phase of phases) {
      expect(exportHoldsRecipeEdits({ ready: true, admitting: false,
        operation: operation({ phase }) }), phase).toBe(true);
    }
    const kinds: Operation['kind'][] = ['profile', 'paths', 'destinations', 'append', 'recover', 'retry_seal', 'restore', 'cancel'];
    for (const kind of kinds) {
      expect(exportHoldsRecipeEdits({ ready: true, admitting: false,
        operation: operation({ kind }) }), kind).toBe(true);
    }
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

import type { Operation } from '../photoExport';

export type ExportEditAdmission = {
  ready: boolean;
  admitting: boolean;
  operation: Operation | null;
};

// Run stage labels describe progress, not SQL custody: publication checkpoints
// can persist into the next tick. Keep inputs stable throughout an ordinary Run.
// SaveRecipe still meets the actor's pre-mutation Busy guard; EditQueue retains
// the latest recipe and reports an error if its finite retry budget expires.
// Unknown state, admission, other operation kinds, cancellation and drain stay held.
export function exportHoldsRecipeEdits({ ready, admitting, operation }: ExportEditAdmission) {
  if (!ready || admitting || !operation?.write_hold) return !ready || admitting;
  return !(operation.kind === 'run'
    && operation.phase === 'running'
    && operation.stage !== 'draining'
    && operation.stage !== 'finished');
}

export type RecipeControlHolds = {
  transitioning: boolean;
  storage: boolean;
  directExport: boolean;
  migration: boolean;
  copy: boolean;
  export: boolean;
};

export const recipeControlsHeld = (holds: RecipeControlHolds) => Object.values(holds).some(Boolean);

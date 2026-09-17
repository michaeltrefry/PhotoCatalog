import type { Operation } from '../photoExport';

export type ExportEditAdmission = {
  ready: boolean;
  admitting: boolean;
  operation: Operation | null;
};

// A rendering Run owns SQL only for a bounded native tick. SaveRecipe still
// meets the actor's authoritative pre-mutation Busy check, and EditQueue keeps
// the user's latest recipe while retrying that typed refusal. Unknown export
// state, admission, recovery, cancellation and drain/yield stages stay held.
export function exportHoldsRecipeEdits({ ready, admitting, operation }: ExportEditAdmission) {
  if (!ready || admitting || !operation?.write_hold) return !ready || admitting;
  return !(operation.kind === 'run'
    && operation.phase === 'running'
    && operation.stage === 'rendering');
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

import { command, type Decimal, type VariantKey } from './bridge';
import type { Recipe, AdjustmentGroup } from './recipe';

export type CopyJob = { sequence: Decimal; id: string; state: 'building' | 'queued' | 'complete' | 'canceled'; total: Decimal; completed: Decimal };
export type CopyTarget = { key: VariantKey; expected_revision: Decimal };
export type CopyName = { filename: string; variant_label: string; available: boolean };
export type CopyItem = { sequence: Decimal; target: CopyTarget; state: 'pending' | 'applied' | 'conflict' | 'incompatible'; applied_revision: Decimal | null; error: string | null; current_revision: Decimal | null; name: CopyName };
export type CopyInspection = { job: CopyJob; source: CopyTarget; recipe: Recipe; digest: string; groups: AdjustmentGroup[]; name: CopyName };
export type CopyOperation = { id: string; job: CopyJob; phase: 'running' | 'paused' | 'cancel_requested' | 'complete' | 'canceled' | 'failed'; error: string | null };
export type CopyRequest =
  | { command: 'begin'; args: { source: VariantKey; expected_revision: Decimal; groups: AdjustmentGroup[] } }
  | { command: 'append'; args: { job: string; expected_total: Decimal; targets: CopyTarget[] } }
  | { command: 'seal'; args: { job: string; expected_total: Decimal } }
  | { command: 'run' | 'job' | 'inspect'; args: { job: string } }
  | { command: 'jobs'; args: { after: Decimal; limit: Decimal } }
  | { command: 'items'; args: { job: string; after: Decimal; limit: Decimal } }
  | { command: 'status'; args: { operation: string | null } }
  | { command: 'cancel'; args: { job: string; operation: string | null } };
export type CopyData = { job: CopyJob; jobs: { rows: CopyJob[]; next: Decimal | null }; items: { rows: CopyItem[]; next: Decimal | null }; inspection: CopyInspection; operation: CopyOperation | null };
export type CopyResponse = { [K in keyof CopyData]: { kind: K; value: CopyData[K] } }[keyof CopyData];
export async function editCopy<K extends keyof CopyData>(catalog: string, request: CopyRequest, kind: K, signal?: AbortSignal): Promise<CopyData[K]> {
  const response = await command({ command: 'edit_copy', args: { catalog, request } }, 'edit_copy', signal);
  if (response.kind !== kind) throw new Error(`Unexpected adjustment copy response: ${response.kind}`);
  return response.value as CopyData[K];
}
export const copyTerminal = (operation: CopyOperation) => ['complete', 'canceled', 'failed'].includes(operation.phase);

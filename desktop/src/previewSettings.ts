import { command, type Decimal, type NativePath } from './bridge';
export type PreviewTier = 'thumbnail' | 'large';
export type PreviewSettingsRequest =
  | { command: 'status' }
  | { command: 'set_budgets'; args: { thumbnail_bytes: Decimal; large_bytes: Decimal } }
  | { command: 'begin_original_root_review'; args: { roots: NativePath[] } }
  | { command: 'step_original_root_review'; args: { review: string; directories: number } }
  | { command: 'begin_relocation'; args: { tier: PreviewTier; destination: NativePath } }
  | { command: 'step_relocation'; args: { tier: PreviewTier; objects: number; bytes: Decimal } };
export interface PreviewSettingsStatus {
  thumbnail_root: NativePath; large_root: NativePath;
  thumbnail_bytes: Decimal; large_bytes: Decimal;
  relocation_pending: boolean; relocation_tier: PreviewTier | null;
  relocation: { source: NativePath; destination: NativePath; phase: string; objects: Decimal | null; bytes: Decimal | null; total_objects: Decimal | null; total_bytes: Decimal | null } | null;
  original_roots: {
    state: 'required' | 'reviewing' | 'ready' | 'stale' | 'blocked';
    roots: NativePath[];
    review: string | null;
    checked_directories: Decimal;
    uncovered: NativePath | null;
    message: string | null;
  };
}
export function previewSettings(catalog: string, request: PreviewSettingsRequest) {
  return command({ command: 'preview_settings', args: { catalog, request } }, 'preview_settings');
}
export function budgetBytes(value: string): string {
  if (!/^[1-9][0-9]*$/.test(value)) throw new Error('Enter a positive whole number of MiB.');
  const bytes = BigInt(value) * 1024n * 1024n;
  if (bytes > 9223372036854775807n) throw new Error('This budget is too large.');
  return bytes.toString();
}
export function reviewedBudget(value: string, current: string): string {
  return value === (BigInt(current) / 1048576n).toString() ? current : budgetBytes(value);
}

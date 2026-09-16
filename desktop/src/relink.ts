import { command, type Decimal, type NativePath, type VariantKey } from './bridge';

export type PathReference = { Native: NativePath } | { LegacyUnix: number[] } | { LegacyWindows: number[] } | { Unspecified: number[] };
export type RelinkScope = { scope: 'prefix'; value: { from: PathReference; destinations: NativePath[] } } | { scope: 'asset'; value: { asset_id: string; destinations: NativePath[] } } | { scope: 'volume'; value: { logical_volume: string; mount_token: string } };
export type RelinkOverride = { target: 'asset'; value: { asset_id: string; candidates: NativePath[] } } | { target: 'source'; value: { source_id: Decimal; candidates: NativePath[] } } | { target: 'prefix'; value: { from: PathReference; destinations: NativePath[] } };
export type RuleCursor = { plan: string; revision: Decimal; stage: 'scope' | 'prefix' | 'asset' | 'source' | 'excluded_asset' | 'excluded_source' | 'end'; position: Decimal; source: Decimal; entity: string };
export type SavedScope = Exclude<RelinkScope, { scope: 'volume' }> | { scope: 'volume'; value: { logical_volume: string; mount_path: NativePath; filesystem: string } };
export type RelinkRule = { kind: 'scope'; value: SavedScope } | { kind: 'override'; value: RelinkOverride };
export type RelinkPlan = { id: string; state: string; revision: Decimal; scanned_through: Decimal; high_water: Decimal; total: Decimal; matched: Decimal; excluded: Decimal; unresolved: Decimal; unresolved_sources: Decimal; unverified: Decimal; user_confirmed: Decimal; confirmation_token: string | null; summary_complete: boolean };
export type RelinkCandidate = { path: NativePath; status: string; detail: string };
export type RelinkItem = { sequence: Decimal; asset_id: string; status: string; detail: string; identity_basis: string; original: PathReference; candidates: RelinkCandidate[]; destination: NativePath | null };
export type RelinkSource = { source_id: Decimal; status: string; detail: string; original: PathReference; candidates: RelinkCandidate[]; destination: NativePath | null };
export type Mount = { token: string; path: NativePath; filesystem: string; identity_available: boolean };
export type Mounts = { rows: Mount[]; complete: boolean; issues: { kind: string; path: NativePath | null; detail: string }[] };
export type Original = { key: VariantKey; status: { asset_id: string; state: string; current: PathReference; candidate: NativePath | null; logical_volume: string | null; detail: string } };
export type RelinkAction = 'prepare' | 'confirm' | 'revise' | 'apply' | 'undo' | 'mounts' | 'original';
export type RelinkOperation = { id: string; action: RelinkAction; phase: 'preparing' | 'observing' | 'draining' | 'applying' | 'undoing' | 'cancel_requested' | 'complete' | 'canceled' | 'failed'; plan: RelinkPlan | null; progress: Decimal; boundary: string | null; write_hold: boolean; result: { kind: 'plan'; value: RelinkPlan } | { kind: 'mounts'; value: Mounts } | { kind: 'original'; value: Original } | null; error: string | null };
export type RelinkRequest =
  | { command: 'begin'; args: { scope: RelinkScope } }
  | { command: 'prepare'; args: { plan: string; revision: Decimal; batch_rows: Decimal } }
  | { command: 'revise'; args: { plan: string; revision: Decimal; changes: RelinkOverride[] } }
  | { command: 'plan'; args: { plan: string } }
  | { command: 'plans'; args: { after: string; limit: Decimal } }
  | { command: 'rules'; args: { plan: string; revision: Decimal; after: RuleCursor | null; limit: Decimal } }
  | { command: 'items'; args: { plan: string; revision: Decimal; after: Decimal; limit: Decimal } }
  | { command: 'sources'; args: { plan: string; revision: Decimal; sequence: Decimal; after: Decimal; limit: Decimal } }
  | { command: 'confirm'; args: { plan: string; revision: Decimal; token: string; acknowledgement: 'no_retained_original_digest' } }
  | { command: 'apply' | 'undo'; args: { plan: string; revision: Decimal } }
  | { command: 'mounts' }
  | { command: 'original'; args: { key: VariantKey } }
  | { command: 'status'; args: { operation: string | null } }
  | { command: 'cancel'; args: { operation: string } };
export type RelinkData = { plan: RelinkPlan; plans: { rows: RelinkPlan[]; next: string | null }; rules: { rows: { rule: RelinkRule; label: string | null }[]; next: RuleCursor | null; scanned: Decimal }; items: { rows: RelinkItem[]; next: Decimal | null }; sources: { rows: RelinkSource[]; next: Decimal | null }; operation: RelinkOperation | null };
export type RelinkResponse = { [K in keyof RelinkData]: { kind: K; value: RelinkData[K] } }[keyof RelinkData];
export async function relink<K extends keyof RelinkData>(catalog: string, request: RelinkRequest, kind: K, signal?: AbortSignal): Promise<RelinkData[K]> {
  const response = await command({ command: 'relink', args: { catalog, request } }, 'relink', signal);
  if (response.kind !== kind) throw new Error(`Unexpected relink response: ${response.kind}`);
  return response.value as RelinkData[K];
}
export const relinkTerminal = (operation: RelinkOperation) => !operation.write_hold && ['complete', 'canceled', 'failed'].includes(operation.phase);

/** Display labels never become the authority submitted for an existing path. */
export function pathLabel(path: NativePath): string {
  if (path.encoding === 'WindowsWide') return path.units.map(unit => String.fromCharCode(unit)).join('');
  try { return new TextDecoder('utf-8', { fatal: true }).decode(Uint8Array.from(path.units)); }
  catch { return `Path with non-UTF8 bytes: ${path.units.map(v => v.toString(16).padStart(2, '0')).join(' ')}`; }
}
export function referenceLabel(reference: PathReference): string {
  if ('Native' in reference) return pathLabel(reference.Native);
  if ('LegacyUnix' in reference) return pathLabel({ encoding: 'UnixBytes', units: reference.LegacyUnix });
  if ('LegacyWindows' in reference) return pathLabel({ encoding: 'WindowsWide', units: reference.LegacyWindows });
  return `Unspecified path encoding: ${reference.Unspecified.map(v => v.toString(16).padStart(2, '0')).join(' ')}`;
}
export function enteredReference(value: string, platform: 'unix' | 'windows'): PathReference {
  if (!value || value.includes('\0') || value.length > 8192) throw new Error('Enter a complete original folder path.');
  if (platform === 'unix') { if (!value.startsWith('/')) throw new Error('macOS and Linux paths must start with /.'); return { LegacyUnix: [...new TextEncoder().encode(value)] }; }
  if (!/^(?:[A-Za-z]:[\\/]|\\\\[^\\]+\\[^\\]+)/.test(value)) throw new Error('Enter an absolute Windows drive or network path.');
  return { LegacyWindows: Array.from({ length: value.length }, (_, index) => value.charCodeAt(index)) };
}
export function identityLabel(basis: string): string {
  return ({ catalog_fingerprint: 'Matches the catalog’s original fingerprint', retained_original_digest: 'Matches a retained complete original digest', user_confirmed_fence: 'Matches a previously confirmed association', unverified: 'No retained original digest; association needs review' } as Record<string, string>)[basis] ?? `Identity evidence: ${basis}`;
}

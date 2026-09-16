import { command, type Decimal, type GridImage, type VariantKey } from './bridge';

export type KeywordKind = 'flat' | 'hierarchical';
export type OrganizationOperation =
  | { operation: 'rating'; value: number }
  | { operation: 'label'; value: string }
  | { operation: 'flag'; value: 'pick' | 'reject' | 'unflagged' }
  | { operation: 'add_keyword' | 'remove_keyword'; kind: KeywordKind; path: string[] }
  | { operation: 'move_keyword'; from: string[]; to: string[] }
  | { operation: 'add_collection' | 'remove_collection'; collection: string };
export type Keyword = { id: Decimal; kind: KeywordKind; parent: Decimal | null; name: string; path: string[] };
export type Collection = { id: string; name: string; revision: Decimal; provenance_json: string };
export type Placement = { collection: string; parent: string | null; position: Decimal };
export type Synonym = { synonym: string; provenance_json: string };
export type Page<T, C> = { rows: T[]; next: C | null };
export type MemberCursor = { position: Decimal; sequence: Decimal };
export type MembershipCursor = MemberCursor & { collection: string; revision: Decimal; ordered: boolean };
export type Member = { key: VariantKey; cursor: MemberCursor; provenance_json: string };
export type ImageIdentity = { image_id: string; key: VariantKey; metadata_revision: Decimal; pixel_generation: Decimal; shared_source_epoch: Decimal; physical_generation: Decimal };
export type BatchItem = { key: VariantKey; expected_revision: Decimal };
export type Job = { id: string; operation: OrganizationOperation; state: string; pending: Decimal; applied: Decimal; failed: Decimal; skipped: Decimal };
export type JobItem = { sequence: Decimal; key: VariantKey; image_id: string; expected_revision: Decimal; status: string; error: string | null; result_revision: Decimal | null };
export type OrganizationRequest =
  | { command: 'keywords'; args: { kind: KeywordKind; parent: Decimal | null; after: Decimal; limit: number } }
  | { command: 'create_keyword'; args: { kind: KeywordKind; path: string[] } }
  | { command: 'delete_keyword'; args: { id: Decimal } }
  | { command: 'synonyms'; args: { keyword: Decimal; after: string; limit: number } }
  | { command: 'add_synonym'; args: { keyword: Decimal; synonym: string } }
  | { command: 'collection'; args: { id: string } }
  | { command: 'collections'; args: { after: string; limit: number } }
  | { command: 'create_collection'; args: { name: string } }
  | { command: 'rename_collection'; args: { id: string; expected_revision: Decimal; name: string } }
  | { command: 'delete_collection'; args: { id: string; expected_revision: Decimal } }
  | { command: 'placement'; args: { collection: string } }
  | { command: 'place_collection'; args: { collection: string; expected_revision: Decimal; parent: string | null; position: Decimal } }
  | { command: 'members'; args: { collection: string; after: MembershipCursor | null; limit: number } }
  | { command: 'identity'; args: { key: VariantKey } }
  | { command: 'set_member'; args: { identity: ImageIdentity; collection: string; position: Decimal } }
  | { command: 'apply'; args: { key: VariantKey; expected_revision: Decimal; operation: OrganizationOperation } }
  | { command: 'begin'; args: { operation: OrganizationOperation } }
  | { command: 'append'; args: { job: string; items: BatchItem[] } }
  | { command: 'seal' | 'step' | 'cancel' | 'job'; args: { job: string } }
  | { command: 'jobs'; args: { after: string; limit: number } }
  | { command: 'items'; args: { job: string; after: Decimal; limit: number } }
  | { command: 'review'; args: { job: string; key: VariantKey; new_revision: Decimal | null } };
export type OrganizationData = {
  keywords: Page<Keyword, Decimal>; keyword_created: { id: Decimal };
  synonyms: Page<Synonym, string>; collection: Collection | null; collections: Page<Collection, string>; collection_created: { id: string };
  placement: Placement; members: Page<Member, MembershipCursor> & { scanned: Decimal }; identity: ImageIdentity;
  changed: { revision: Decimal }; acknowledged: undefined; job: Job; jobs: Page<Job, string>; items: Page<JobItem, Decimal>;
};
export type OrganizationResponse = { [K in keyof OrganizationData]: { kind: K; value: OrganizationData[K] } }[keyof OrganizationData];
export async function organize<K extends keyof OrganizationData>(catalog: string, request: OrganizationRequest, expected: K, signal?: AbortSignal): Promise<OrganizationData[K]> {
  const result = await command({ command: 'organization', args: { catalog, request } }, 'organization', signal);
  if (result.kind !== expected) throw new Error('Unexpected organization response. Refresh before retrying.');
  return result.value as OrganizationData[K];
}
export function nonnegativeDecimal(value: string): Decimal {
  if (!/^(0|[1-9][0-9]*)$/.test(value) || BigInt(value) > 9223372036854775807n) throw new Error('Position must be a whole number from 0 to 9223372036854775807.');
  return value;
}
export function selectionPage(rows: GridImage[]): BatchItem[] {
  if (!rows.length || rows.length > 100) throw new Error('Choose 1 to 100 photos from the current page.');
  const seen = new Set<string>();
  return rows.map(row => {
    const id = JSON.stringify([row.key.asset_id, row.key.variant_id]);
    if (seen.has(id)) throw new Error('The same photo variant was selected twice.');
    seen.add(id);
    return { key: { ...row.key }, expected_revision: row.metadata_revision };
  });
}
export function operationName(op: OrganizationOperation): string {
  switch (op.operation) {
    case 'rating': return `Set ${op.value} stars`;
    case 'label': return op.value ? `Set label “${op.value}”` : 'Clear color label';
    case 'flag': return `Set flag: ${op.value}`;
    case 'add_keyword': return `Add keyword: ${op.path.join(' / ')}`;
    case 'remove_keyword': return `Remove keyword: ${op.path.join(' / ')}`;
    case 'move_keyword': return `Move keyword: ${op.from.join(' / ')} → ${op.to.join(' / ')}`;
    case 'add_collection': return 'Add to collection';
    case 'remove_collection': return 'Remove from collection';
  }
}

export function jobName(job: Job, collection?: Collection | null): string {
  if (job.operation.operation === 'add_collection' || job.operation.operation === 'remove_collection') {
    const action = job.operation.operation === 'add_collection' ? 'Add to' : 'Remove from';
    return collection ? `${action} collection “${collection.name}”` : `${action} collection (${collection === null ? 'deleted or unavailable' : 'loading name…'})`;
  }
  return operationName(job.operation);
}

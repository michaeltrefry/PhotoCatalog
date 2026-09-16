import { command, type Decimal, type VariantKey } from './bridge';
import type { ImageIdentity } from './organization';

export type TextReference =
  | { kind: 'effective'; field: string }
  | { kind: 'candidate'; model: Decimal; field: string }
  | { kind: 'source'; source: Decimal; field: 'kind' | 'display' | 'association' | 'availability' | 'locator' }
  | { kind: 'observation'; observation: Decimal; field: 'revision' | 'status' | 'issues' | 'provenance' | 'created_at' }
  | { kind: 'model'; model: Decimal; field: 'descriptor' | 'projection' | 'error' }
  | { kind: 'packet'; observation: Decimal; ordinal: Decimal }
  | { kind: 'decision'; decision: Decimal; field: 'action' | 'detail' | 'created_at' }
  | { kind: 'file_instance'; instance: Decimal; field: 'provenance' | 'observed_at' };
export type BlobSource = { kind: 'packet'; observation: Decimal; ordinal: Decimal } | { kind: 'model'; id: Decimal };
export type RetainedText = { bytes: Decimal; inline: string | null; reference: TextReference };
export type MetadataPage<T> = { rows: T[]; next: string | null; scanned: Decimal };
export type MetadataField = { name: string; value: RetainedText | null; conflicted: boolean; selected_model: Decimal | null };
export type MetadataCandidate = { model: Decimal; source: Decimal; observation: Decimal; ordinal: Decimal; ambiguous: boolean; semantic_hash: string; value: RetainedText };
export type MetadataSource = { id: Decimal; kind: RetainedText; display: RetainedText; association: RetainedText; availability: RetainedText; locator: RetainedText; observation: Decimal | null; status: RetainedText | null; issues: RetainedText | null };
export type Observation = { id: Decimal; source: Decimal; revision: RetainedText; status: RetainedText; issues: RetainedText; provenance: RetainedText; created_at: RetainedText; current: boolean };
export type MetadataModel = { id: Decimal; ordinal: Decimal; blob_hash: string; bytes: Decimal; descriptor: RetainedText; projection: RetainedText; error: RetainedText | null };
export type MetadataPacket = { ordinal: Decimal; blob_hash: string; bytes: Decimal; descriptor: RetainedText };
export type MetadataDecision = { id: Decimal; revision: Decimal; action: RetainedText; detail: RetainedText; created_at: RetainedText };
export type FileInstance = { id: Decimal; source: Decimal; provenance: RetainedText; observed_at: RetainedText };
export type MetadataChunk = { bytes: number[]; offset: Decimal; total: Decimal; next: Decimal | null; blake3: string | null; verified: boolean; inspected_bytes: Decimal };
export type RecordRole = 'row' | 'table' | 'entity';
export type HistoryDirection = 'Outgoing' | 'Incoming';
export type ImportedRow = { record: Decimal; source_key_json: string; source_id: string; entity_record: Decimal; table_record: Decimal; cells_field: string; cells_json_bytes: Decimal; classification: string };
export type ImportedRelation = { reference_record: Decimal; source_id: string; field: string; target_table: string; target_key: string; source: ImportedRow | null; target: ImportedRow | null; compatibility: string; anchor_json: string | null };
export type ImportedPage = { key: VariantKey; input: string; anchor_json: string; row: ImportedRow; relations: ImportedRelation[]; next: string | null; coverage_complete: boolean; keys_complete: boolean; adobe_rendering_equivalent: boolean };
export type ImportedField = { name: string; representation: string; bytes: Decimal; scalar_json: string | null };
export type ImportedColumn = { ordinal: Decimal; name: string; cell_type: string | null; bytes: Decimal | null };
export type ImportedColumns = { row: ImportedRow; columns: MetadataPage<ImportedColumn>; types_complete: boolean; reason: string };
export type AdobeProperty = { path_json: string; namespace: string | null; name: string; lexical: string; start: Decimal; end: Decimal; value_json: string; disposition: string; reason: string };
export type AdobePage = { row: ImportedRow; compatibility: string; reason: string; input_json: string | null; coordinate_space: string | null; properties: MetadataPage<AdobeProperty>; missing: string[]; failure: unknown | null; contribution_json: string | null; adobe_rendering_equivalent: boolean };

type ImagePage = { identity: ImageIdentity; after: string | null; limit: number };
export type MetadataRequest =
  | { command: 'identity'; args: { key: VariantKey } }
  | { command: 'fields'; args: ImagePage }
  | { command: 'candidates'; args: { identity: ImageIdentity; field: string; after: string | null; limit: number } }
  | { command: 'sources' | 'observations' | 'decisions' | 'file_instances'; args: ImagePage }
  | { command: 'models' | 'packets'; args: ImagePage & { observation: Decimal } }
  | { command: 'resolve'; args: { key: VariantKey; expected_revision: Decimal; field: string; model: Decimal } }
  | { command: 'text_chunk'; args: { identity: ImageIdentity; reference: TextReference; offset: Decimal; length: number } }
  | { command: 'blob_chunk'; args: { key: VariantKey; source: BlobSource; offset: Decimal; length: number } }
  | { command: 'import_history'; args: { key: VariantKey; anchor_json: string | null; direction: HistoryDirection; after_json: string | null; limit: number } }
  | { command: 'import_fields'; args: { key: VariantKey; anchor_json: string; role: RecordRole; after: string; limit: number } }
  | { command: 'import_columns'; args: { key: VariantKey; anchor_json: string; after: Decimal; limit: number } }
  | { command: 'import_chunk'; args: { key: VariantKey; anchor_json: string; role: RecordRole; field: string; offset: Decimal; length: number } }
  | { command: 'adobe_properties'; args: { key: VariantKey; anchor_json: string; column: string; settings_path_json: string; after: Decimal; limit: number } };
export type MetadataData = {
  identity: ImageIdentity;
  fields: MetadataPage<MetadataField>;
  candidates: MetadataPage<MetadataCandidate>;
  sources: MetadataPage<MetadataSource>;
  observations: MetadataPage<Observation>;
  models: MetadataPage<MetadataModel>;
  packets: MetadataPage<MetadataPacket>;
  decisions: MetadataPage<MetadataDecision>;
  file_instances: MetadataPage<FileInstance>;
  changed: { revision: Decimal };
  chunk: MetadataChunk;
  import_history: ImportedPage;
  import_fields: MetadataPage<ImportedField>;
  import_columns: ImportedColumns;
  adobe_properties: AdobePage;
};
export type MetadataResponse = { [K in keyof MetadataData]: { kind: K; value: MetadataData[K] } }[keyof MetadataData];

export async function metadata<K extends keyof MetadataData>(catalog: string, request: MetadataRequest, kind: K, signal?: AbortSignal): Promise<MetadataData[K]> {
  const response = await command({ command: 'metadata', args: { catalog, request } }, 'metadata', signal);
  if (response.kind !== kind) throw new Error(`Unexpected metadata response: ${response.kind}`);
  return response.value as MetadataData[K];
}

/** Format text without converting retained numeric values through JavaScript Number. */
export function readableValue(value: string): string {
  if (value.startsWith('"')) {
    try { const decoded: unknown = JSON.parse(value); if (typeof decoded === 'string') return decoded; } catch { /* Keep original text. */ }
  }
  return value;
}

export function chunkHex(bytes: number[], offset: string): string {
  const start = BigInt(offset);
  const lines: string[] = [];
  for (let index = 0; index < bytes.length; index += 16) {
    const row = bytes.slice(index, index + 16);
    lines.push(`${(start + BigInt(index)).toString(16).padStart(8, '0')}  ${row.map(byte => byte.toString(16).padStart(2, '0')).join(' ').padEnd(47)}  ${row.map(byte => byte >= 32 && byte <= 126 ? String.fromCharCode(byte) : '.').join('')}`);
  }
  return lines.join('\n');
}

export type SettingsStep = { kind: 'name'; name: string } | { kind: 'index'; index: string } | { kind: 'xml'; namespace: string; name: string; ordinal: string };
/** Serialize numeric path components lexically so full u64 indices stay exact. */
export function settingsPath(steps: SettingsStep[]): string {
  if (steps.length > 16) throw new Error('Settings paths support at most 16 steps.');
  const integer = (value: string) => { if (!/^(0|[1-9][0-9]*)$/.test(value) || BigInt(value) > 18446744073709551615n) throw new Error('Enter a whole nonnegative index.'); return value; };
  return `[${steps.map(step => step.kind === 'name' ? JSON.stringify({ kind: 'name', value: step.name }) : step.kind === 'index' ? `{"kind":"index","value":${integer(step.index)}}` : `{"kind":"xml","value":{"namespace":${JSON.stringify(step.namespace)},"name":${JSON.stringify(step.name)},"ordinal":${integer(step.ordinal)}}}`).join(',')}]`;
}

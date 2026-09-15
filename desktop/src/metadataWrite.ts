/** Proposed metadata_write v1. Source/design only. Outer command: {command:'metadata_write',args:{catalog,request}}.
 * Rust uses tagged enums with deny_unknown_fields. All authority integers serialize as canonical decimal strings.
 * Canonical UUID attempts are caller-generated; tokens and cursors are opaque server-generated values.
 */
export type Decimal = string;
export type NativePath = { encoding: 'UnixBytes'; units: number[] } | { encoding: 'WindowsWide'; units: number[] };
export type VariantKey = { asset_id: string; variant_id: string };
export type ImageIdentity = { image_id: string; key: VariantKey; metadata_revision: Decimal; pixel_generation: Decimal; shared_source_epoch: Decimal; physical_generation: Decimal };
export type Edit =
 | { operation:'set'; namespace:string; path:string; value:string }
 | { operation:'remove'; namespace:string; path:string }
 | { operation:'append'; namespace:string; path:string; value:string; ordered:boolean }
 | { operation:'localized'; namespace:string; path:string; language:string; value:string };
export type Limits = { input_bytes:Decimal; existing_file_bytes:Decimal; evidence_bytes:Decimal; evidence_packets:Decimal; alias_directories:Decimal; alias_candidates:Decimal };
export type Options = { defaults:Limits; input_max:Decimal; packet_max:Decimal; edits_max:Decimal; page_max:number; scan_max:Decimal; response_bytes:Decimal; chunk_max:number; path_bytes:Decimal; receipt_bytes:Decimal };
export type Page<T> = { rows:T[]; next:string|null; scanned:Decimal };
export type Bytes = { bytes:number[]; offset:Decimal; total:Decimal; next:Decimal|null; blake3:string; verified:boolean };
export type Ref = { token:string; bytes:Decimal; blake3:string; media_type:string };
export type Text = { inline:string|null; reference:Ref|null; bytes:Decimal };
export type Input = { token:string; generation:Decimal; bytes:Decimal; expected_bytes:Decimal; expected_blake3:string|null; last_offset:Decimal|null; last_chunk_blake3:string|null; sealed:boolean; blake3:string|null };
export type Review = { token:string; digest:string; identity:ImageIdentity; base_model:Decimal|null; input_blake3:string; edits:Decimal; changed_fields:Decimal; packet:Ref; issues:Text };
export type ReviewField = { field:string; before:Text|null; after:Text|null; removed:boolean; semantic_changed:boolean };
export type FileRevision = { bytes:Decimal; digest:string; modified_ns:Decimal; identity:[Decimal,Decimal] };
export type SidecarReceipt = { state:'Published'|'Restored'|'Conflict'|'Recoverable'; destination:NativePath; recovery_directory:NativePath; captured_original:NativePath|null; detail:Text; exact:Ref };
export type SidecarPlan = { operation:string; version:Decimal; owner:{kind:'image'; identity:ImageIdentity}|{kind:'legacy_asset'; asset_id:string; revision:Decimal}; base_model:Decimal; destination:NativePath; expected:FileRevision|null; payload_bytes:Decimal; payload_digest:string; authority:Ref; payload:Ref; current:boolean; receipt:SidecarReceipt|null };
export type RecoveryEntry = { directory:NativePath; kind:'known'|'unknown'|'preparing'|'invalid'; operation:string|null; plan_digest:string|null; detail:Text };
export type Change = { revision:Decimal; observation_id:Decimal; model_ids:Decimal[]; changed:boolean };
export type EvidenceReceipt = { destination:NativePath; bytes:Decimal; blake3:string|null; state:'complete'|'partial'; detail:Text };
export type Result =
 | { kind:'input'; value:Input }
 | { kind:'released'; value:null }
 | { kind:'review'; value:Review }
 | { kind:'changed'; value:Change }
 | { kind:'resolved'; value:{revision:Decimal} }
 | { kind:'sidecar_plan'; value:SidecarPlan }
 | { kind:'sidecar_receipt'; value:SidecarReceipt }
 | { kind:'discovery'; value:{token:string; directory:NativePath} }
 | { kind:'paths'; value:{projected:Decimal; pending:Decimal; unbound:Decimal} }
 | { kind:'evidence'; value:EvidenceReceipt };
export type Error = { code:'invalid_input'|'stale'|'busy'|'resource_limit'|'canceled'|'unavailable'|'conflict'|'internal'; message:string; detail:Ref|null };
export type Operation = { id:string; attempt:string; request_digest:string; epoch:Decimal; kind:Action['kind']; phase:'running'|'complete'|'failed'|'canceled'; stage:'admitting'|'reading'|'preparing'|'waiting_writer'|'committing'|'hashing'|'capturing'|'publishing'|'restoring'|'draining'; cancel_requested:boolean; progress:Decimal; result:Result|null; error:Error|null };
export type Status = { catalog:string; epoch:Decimal; operation:Operation|null; write_hold:boolean; closing:boolean; input:Input|null; review:Review|null };
export type DurableReceipt = { attempt:string; request_digest:string; kind:'edit'|'resolve'|'sidecar_plan'|'sidecar_apply'|'sidecar_recover'|'sidecar_restore'; identity:ImageIdentity|null; legacy_asset:string|null; result:Result; created_at:string };
/** Every action is independently observable by exact attempt before its invoke reply. No long direct mutation reply owns the App gate. */
export type Action =
 | { kind:'input_begin'; bytes:Decimal; expected_blake3:string|null; limits:Limits }
 | { kind:'input_append'; token:string; generation:Decimal; offset:Decimal; bytes:number[] }
 | { kind:'input_finish'; token:string; generation:Decimal; bytes:Decimal; blake3:string }
 | { kind:'input_discard'; token:string; generation:Decimal }
 | { kind:'prepare'; identity:ImageIdentity; base_model:Decimal|null; input:string; input_blake3:string }
 | { kind:'release'; token:string }
 | { kind:'commit'; review:string; review_digest:string }
 | { kind:'resolve'; identity:ImageIdentity; field:string; model:Decimal }
 | { kind:'sidecar_plan'; identity:ImageIdentity; base_model:Decimal; destination:NativePath; limits:Limits }
 | { kind:'sidecar_apply'; operation:string; authority_blake3:string; overwrite_ack:boolean; limits:Limits }
 | { kind:'sidecar_recover'; operation:string; authority_blake3:string; recovery_directory:NativePath; may_publish_ack:boolean; limits:Limits }
 | { kind:'sidecar_restore'; operation:string; authority_blake3:string; recovery_directory:NativePath; limits:Limits }
 | { kind:'discover'; directory:NativePath }
 | { kind:'reconcile_paths'; rows:number }
 | { kind:'evidence_export'; identity:ImageIdentity; observation:Decimal; destination:NativePath; limits:Limits };
export type Request =
 | { command:'options' }
 | { command:'status'; args:{operation:string|null} }
 | { command:'cancel'; args:{operation:string; epoch:Decimal} }
 | { command:'start'; args:{attempt:string; action:Action} }
 | { command:'input_status'; args:{token:string; generation:Decimal} }
 | { command:'receipt'; args:{attempt:string} }
 | { command:'review'; args:{token:string; digest:string} }
 | { command:'review_fields'; args:{token:string; digest:string; after:string|null; limit:number} }
 | { command:'chunk'; args:{reference:Ref; offset:Decimal; length:number} }
 | { command:'plans'; args:{owner:VariantKey|null; after:string|null; limit:number} }
 | { command:'plan'; args:{operation:string} }
 | { command:'recovery_entries'; args:{token:string; after:string|null; limit:number} };
export type Response =
 | { kind:'options'; value:Options }
 | { kind:'status'; value:Status }
 | { kind:'admitted'; value:{operation:string; attempt:string; request_digest:string; epoch:Decimal} }
 | { kind:'input'; value:Input }
 | { kind:'receipt'; value:DurableReceipt|null }
 | { kind:'review'; value:Review }
 | { kind:'review_fields'; value:Page<ReviewField> }
 | { kind:'chunk'; value:Bytes }
 | { kind:'plans'; value:Page<SidecarPlan> }
 | { kind:'plan'; value:SidecarPlan|null }
 | { kind:'recovery_entries'; value:Page<RecoveryEntry> };
/** Existing metadata endpoint remains available unchanged for every retained/conflict/history/imported/Adobe query.
 * Close is the existing outer Catalog Close; it cancels all consumers before join and does not publish Closed until drained.
 * Operations and input/review tokens are session scoped. Durable receipt/sidecar operation IDs survive Close/reopen.
 * A Ref is opaque and owned: the server never treats caller bytes/digest/path as sufficient authority.
 * Query pages/chunks never authorize mutation. Only reviewed owned token/exact stored plan plus current transaction identity can do so.
 */

import { command } from './bridge';
export type Data = { [R in Response as R['kind']]: R['value'] };
export async function metadataWrite<K extends keyof Data>(catalog:string, request:Request, kind:K, signal?:AbortSignal):Promise<Data[K]> {
  const response=await command({command:'metadata_write',args:{catalog,request}},'metadata_write',signal);
  if(response.kind!==kind) throw new Error(`Unexpected metadata write response: ${response.kind}`);
  return response.value as Data[K];
}
export const metadataWriteTerminal=(operation:Operation)=>['complete','failed','canceled'].includes(operation.phase);

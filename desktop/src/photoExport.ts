import { command } from './bridge';
// Typed desktop export protocol; installed workflow qualification remains separate.
// Inner response is wrapped by {kind:'export',data:ExportResponse}.
export type I64 = string;
export type U64 = string;
export type U128 = string;
export type NativePath = {encoding:'UnixBytes';units:number[]} | {encoding:'WindowsWide';units:number[]};
export type VariantKey = {asset_id:string;variant_id:string};
export type Name = {filename:string;variant_label:string;available:boolean};
export type TargetKey = {key:VariantKey;expected_revision:I64};
export type Metadata = {mode:'omit'} | {mode:'resolved';expected_revision:I64;base_model:I64|null};
export type Format = {format:'jpeg';quality:number} | {format:'png';depth:'eight'|'sixteen'} | {format:'tiff';depth:'eight'|'sixteen'|'float32'};
export type Size = {mode:'original'} | {mode:'fit';width:number;height:number;allow_upscale:boolean};
export type Alpha = {mode:'preserve'} | {mode:'composite';linear_rgb:[number,number,number]};
export type Profile = {kind:'srgb'} | {kind:'linear_srgb'} | {kind:'icc';token:string};
export type Output = {size:Size;format:Format;profile:Profile;alpha:Alpha};
export type AliasLimits = {directories:U64;candidates:U64};
export type Budgets = {max_original_bytes:U64;max_payload_bytes:U64;alias_limits:AliasLimits};
export type RenderLimits = {max_pixels:U64;max_allocation_bytes:U64;max_live_bytes:U64};
export type DecodeLimits = {max_encoded_bytes:U64;max_intermediate_pixels:U64;max_allocation_bytes:U64};
export type ExecutionLimits = {
 worker_bytes:U64;working_bytes:U64;
 render:{decode:DecodeLimits;render:RenderLimits;encode:{render:RenderLimits;max_metadata_bytes:U64;row_buffer_bytes:U64};max_encoded_extent:U64};
};
export type Options = {
 budgets:Budgets;execution:ExecutionLimits;
 page_rows:U64;page_bytes:U64;path_bytes:U64;profile_bytes:U64;
 profile_tokens:U64;profile_total_bytes:U64;result_rows:U64;
};
export type FileRevision = {bytes:U64;digest:string;modified_ns:U128;identity:[U64,U64]};
export type Receipt = {state:'published'|'restored'|'conflict'|'recoverable';destination:NativePath;recovery_directory:NativePath;captured_original:NativePath|null;detail:string};
export type Job = {sequence:I64;id:string;state:'building'|'queued'|'complete'|'canceled';total:I64;completed:I64};
export type Item = {sequence:I64;key:VariantKey;name:Name;destination:NativePath;state:'pending'|'rendering'|'sealed'|'published'|'failed'|'canceled'|'restored';attempt:string|null;authority:string;error:string|null;receipt:Receipt|null};
export type ImageIdentity = {image_id:string;key:VariantKey;metadata_revision:I64;pixel_generation:I64;shared_source_epoch:I64;physical_generation:I64};
export type RenderIdentity = {image_identity:ImageIdentity|null;source:{asset_id:string;generation:I64;fingerprint:string|null;state:string;metadata_revision:I64};key:VariantKey;revision:I64;recipe_digest:string};
export type StoredProfile = {kind:'srgb'}|{kind:'linear_srgb'}|{kind:'icc';blob:string;bytes:U64;linear:boolean|null};
export type Plan = {
 job:string;sequence:I64;authority:string;plan_bytes:U64;version:number;renderer_identity:string;
 name:Name;identity:RenderIdentity;original:NativePath;original_revision:FileRevision;
 output:{size:Size;format:Format;profile:StoredProfile;alpha:Alpha};
 metadata:Metadata;xmp_blob:{digest:string;bytes:U64}|null;
 destination:{version:number;operation:string;destination:NativePath;expected:FileRevision|null;max_existing_bytes:U64};
 budgets:Budgets;item:Item;
};
// Chunk is exact persisted plan UTF-8 text, bounded at UTF-8 boundaries; offset/next
// are byte offsets. It contains only stored blob digests, never ICC/XMP payloads.
export type PlanChunk = {job:string;sequence:I64;authority:string;offset:U64;next:U64|null;total_bytes:U64;text:string};
export type ProfileAdmission = {token:string;name:string;bytes:U64;blake3:string;linear:boolean};
export type Paths = {projected:U64;pending:boolean;unbound:U64};
export type Naming = {prefix:string;suffix:string;variant_suffix:boolean;sequence_start:U64|null};
export type Destination = {target:TargetKey;name:Name;destination:NativePath|null;error:string|null};
export type DestinationResult = {token:string;total:U64};
export type Recovery = {fenced:U64;complete:boolean};
export type ResultValue =
 | {kind:'profile';value:ProfileAdmission}
 | {kind:'paths';value:Paths}
 | {kind:'destinations';value:DestinationResult}
 | {kind:'appended';value:{job:Job;item:Item}}
 | {kind:'job';value:Job}
 | {kind:'recovery';value:Recovery}
 | {kind:'receipt';value:{job:Job;sequence:I64;authority:string;receipt:Receipt}};
export type OperationKind = 'profile'|'paths'|'destinations'|'append'|'run'|'recover'|'retry_seal'|'restore'|'cancel';
export type OperationPhase = 'running'|'waiting_for_previews'|'paused'|'cancel_requested'|'complete'|'canceled'|'failed';
export type Operation = {
 id:string;kind:OperationKind;phase:OperationPhase;
 job:Job|null;sequence:I64|null;
 stage:'opening'|'planning'|'hashing'|'alias'|'waiting_for_previews'|'rendering'|'accepting'|'intent_committed'|'captured'|'capture_verified'|'linked'|'finalizing'|'installed_verified'|'recovering'|'restoring'|'yielding'|'finished';
 stream_bytes:U64|null;processed:U64;write_hold:boolean;
 result:ResultValue|null;error:string|null;
};
export type Request =
 | {command:'options'}
 | {command:'profile';args:{path:NativePath}}
 | {command:'profile_release';args:{token:string}}
 | {command:'begin'}
 | {command:'paths';args:{limit:U64}}
 | {command:'destinations';args:{directory:NativePath;targets:TargetKey[];format:Format;naming:Naming}}
 | {command:'destination_rows';args:{token:string;after:U64;limit:U64}}
 | {command:'result_release';args:{token:string}}
 | {command:'append';args:{job:string;expected_total:I64;target:TargetKey&{destination:NativePath;overwrite:boolean;metadata:Metadata};output:Output;budgets:Budgets|null}}
 | {command:'seal';args:{job:string;expected_total:I64}}
 | {command:'job';args:{job:string}}
 | {command:'jobs';args:{after:I64;limit:U64}}
 | {command:'items';args:{job:string;after:I64;limit:U64}}
 | {command:'plan';args:{job:string;sequence:I64}}
 | {command:'plan_chunk';args:{job:string;sequence:I64;authority:string;offset:U64;bytes:U64}}
 | {command:'run';args:{job:string;limits:ExecutionLimits|null;max_items:U64;max_seconds:U64}}
 | {command:'status';args:{operation:string|null}}
 | {command:'cancel';args:{job:string|null;operation:string|null}}
 | {command:'yield';args:{job:string;operation:string}}
 | {command:'recover';args:{directories:U64;limits:ExecutionLimits|null}}
 | {command:'retry_seal';args:{job:string;sequence:I64;authority:string}}
 | {command:'restore';args:{job:string;sequence:I64;authority:string}};
export type Response =
 | {kind:'options';value:Options}
 | {kind:'released';value:{token:string}}
 | {kind:'job';value:Job}
 | {kind:'jobs';value:{rows:Job[];next:I64|null}}
 | {kind:'items';value:{rows:Item[];next:I64|null}}
 | {kind:'plan';value:Plan}
 | {kind:'plan_chunk';value:PlanChunk}
 | {kind:'destinations';value:{rows:Destination[];next:U64|null;total:U64}}
 | {kind:'operation';value:Operation|null};

export type Data = { [R in Response as R['kind']]: R['value'] };
export async function photoExport<K extends keyof Data>(catalog: string, request: Request, kind: K, signal?: AbortSignal): Promise<Data[K]> {
  const response = await command({ command: 'export', args: { catalog, request } }, 'export', signal);
  if (response.kind !== kind) throw new Error(`Unexpected photo export response: ${response.kind}`);
  return response.value as Data[K];
}
export const terminal = (operation: Operation) => ['complete', 'canceled', 'failed', 'paused'].includes(operation.phase);

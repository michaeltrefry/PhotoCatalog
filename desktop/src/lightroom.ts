/** Inspection lifetime is independent of Catalog Close. Only explicit Workbench
 * Close or application shutdown cancels it. No request here implies import. */
import {command,type Decimal,type NativePath} from './bridge';
export type Guard = {workbench:string;generation:string;operation:string};
export type InputPurpose = 'Inventory'|'SelectionRequest'|'Approval';
export type Request =
 | {kind:'Options'}
 | {kind:'Open';attempt:string;root:NativePath;mode:'Create'|'OpenExisting';capture_staging:NativePath;limits:WorkbenchLimits}
 | {kind:'Status';workbench:string|null;attempt:string|null}
 | {kind:'Action';guard:Guard;action:Action}
 | {kind:'Read';guard:Guard;query:Query}
 | {kind:'Cancel';guard:Guard}
 | {kind:'Close';workbench:string}
 | {kind:'Result';guard:Guard;token:string;offset:Decimal;limit:Decimal}
 | {kind:'InputBegin';guard:Guard;purpose:InputPurpose;total_bytes:Decimal;expected_blake3:string|null}
 | {kind:'InputAppend';guard:Guard;input:string;offset:Decimal;fragment:string}
 | {kind:'InputFinish'|'InputDiscard';guard:Guard;input:string}
 | {kind:'InputStatus';guard:Guard;input:string|null};
export type Options={envelope_bytes:Decimal;chunk_bytes:Decimal;input_slots:Decimal;input_owned_factor:Decimal;minimum_nonfinal_chunk_bytes:Decimal;workbench:WorkbenchLimits;inspection:InspectionLimits;selection:SelectionLimits};
export type ResultPage=Guard & {attempt:string;token:string;offset:Decimal;next:Decimal|null;total_bytes:Decimal;json_fragment:string};
export type WorkbenchLimits = { request_bytes: Decimal; result_bytes: Decimal; page_bytes: Decimal; row_bytes: Decimal; native_path_units: Decimal; vm_steps: Decimal; deadline_ms: Decimal };
export type InspectionLimits = { max_files: Decimal; max_depth: Decimal; max_file_bytes: Decimal; max_total_bytes: Decimal; max_cell_bytes: Decimal };
export type SelectionLimits = { review_bytes: Decimal; row_bytes: Decimal; page_bytes: Decimal; native_path_units: Decimal; snapshot_bytes: Decimal; vm_steps: Decimal; deadline_ms: Decimal };
export type Action =
 | {kind:'Discover';root:NativePath;limits:InspectionLimits}
 | {kind:'Capture';source:NativePath;output:NativePath;include_auxiliary:boolean;closed_application_evidence:string|null;limits:InspectionLimits}
 | {kind:'RegisterInventory';input:string}
 | {kind:'AddCapture';directory:NativePath}
 | {kind:'Resume';revision:string;max_rows:Decimal}
 | {kind:'InspectOriginals';revision:string;limit:Decimal;inspection:'MetadataOnly'|'Packets'}
 | {kind:'AssignFamily';revision:string;family:string;reason:string}
 | {kind:'Choose';family:string;revision:string;expected_evidence:string;reason:string}
 | {kind:'PrepareSelection';input:string;limits:SelectionLimits}
 | {kind:'Seal';review_token:string;approval_blake3:string;input:string;output:NativePath}
 | {kind:'ReleaseReview'};
export type Query =
 | {kind:'CaptureManifest';directory:NativePath}
 | {kind:'Rows';revision:string;table:string|null;after:Decimal;limit:Decimal}
 | {kind:'Report';revision:string}
 | {kind:'Paths'|'Issues'|'Packets'|'MetadataConflicts';revision:string;after:Decimal;limit:Decimal}
 | {kind:'PacketBytes';revision:string;sequence:Decimal;decoded:boolean;offset:Decimal;limit:Decimal}
 | {kind:'GlobalIdConflicts';left:string;right:string;after_left:string;after_right:string;limit:Decimal}
 | {kind:'PathCollisions';left:string;right:string;after_left:Decimal;after_right:Decimal;limit:Decimal}
 | {kind:'Families'|'SelectionSummary'}
 | {kind:'SelectionSources';review_token:string;revision:string;after:Decimal;limit:Decimal}
 | {kind:'SelectionPreparation';review_token:string;document:{kind:'Manifest';revision:string}|{kind:'OriginalEvidence';revision:string;source_id:string};offset:Decimal;limit:Decimal}
 | {kind:'SelectionPage';review_token:string;collection:'Families'|'Captures'|'UninspectedCandidates'|'ConflictSample'|'PathCollisionSample';after:Decimal;limit:Decimal};
export type Status = { attempt:string;workbench:string;generation:string;operation:string;phase:'Opening'|'Running'|'Complete'|'Failed'|'CancelRequested'|'Canceled'|'Closing'|'Closed';initialized:boolean;closed:boolean;root:NativePath;limits:WorkbenchLimits;processed:Decimal;result_token:string|null;result_bytes:Decimal;review_token:string|null;capture_pid:Decimal|null;capture_staging:NativePath|null;error:string|null };
export type InputStatus = {guard:Guard;attempt:string;input:string;purpose:InputPurpose;total_bytes:Decimal;received_bytes:Decimal;blake3:string|null;expected_blake3:string|null;complete:boolean};
export type Response = {kind:'Options';value:Options} | {kind:'Status';value:Status|null} | {kind:'Result';value:ResultPage} | {kind:'Input';value:InputStatus|null};

export async function lightroom(request:Request,signal?:AbortSignal):Promise<Response> {
  return command({command:'lightroom',args:{request}},'lightroom',signal);
}
export const inspectionTerminal=(s:Status)=>s.closed || ['Complete','Failed','Canceled','Closed'].includes(s.phase);

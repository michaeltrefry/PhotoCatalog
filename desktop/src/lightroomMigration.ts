import { command, type Decimal, type NativePath } from './bridge';
import { decimal, encodeInput, inputChunk } from './lightroomWorkflow';

export type Guard = { session: string; generation: string; operation: string };
export type InputRole = 'operation' | 'seal' | 'approval' | 'policy' | 'repair_request' | 'supplement_requests' | 'execution_authorization';
export type Operation =
  | { operation: 'run'; approval_blake3: string; max_steps: Decimal; max_seconds: Decimal; source_open_ms: Decimal; artifact_open_ms: Decimal; max_artifact_bytes: Decimal }
  | { operation: 'status'; run: string }
  | { operation: 'prepare_supplements' }
  | { operation: 'repair_current'; max_steps: Decimal; max_seconds: Decimal; source_open_ms: Decimal }
  | { operation: 'repair_status'; repair: string }
  | { operation: 'repair_keywords'; max_steps: Decimal; max_seconds: Decimal; source_open_ms: Decimal }
  | { operation: 'keyword_repair_status'; repair: string };
export type PartDescriptor = { role: InputRole; bytes: Decimal; blake3: string };
export type Header = { catalog: string | null; destination: NativePath; operation: Operation; parts: PartDescriptor[]; timeout_ms: Decimal };
export type Request =
  | { action: 'begin'; operation: string; header: Header }
  | { action: 'upload'; guard: Guard; role: InputRole; offset: Decimal; text: string }
  | { action: 'finish'; guard: Guard; role: InputRole; blake3: string }
  | { action: 'act' | 'status' | 'cancel' | 'retry_drain' | 'discard'; guard: Guard }
  | { action: 'result_page'; guard: Guard; page: Decimal; offset: Decimal; maximum_bytes: Decimal };
export type Phase = 'uploading' | 'ready' | 'running' | 'cancel_requested' | 'drain_pending' | 'complete' | 'failed';
export type Failure = { code: string; detail: string; required: Decimal | null; available: Decimal | null; poisoned: boolean; outcome_unknown: boolean };
export type ResultIdentity = { bytes: Decimal; blake3: string; pages: Decimal };
export type Snapshot = { guard: Guard; phase: Phase; catalog: string | null; uploaded: Decimal; next_role: InputRole | null; progress: [string, Decimal, Decimal | null] | null; failure: Failure | null; result: ResultIdentity | null };
export type Response =
  | { kind: 'status'; data: Snapshot }
  | { kind: 'page'; data: { guard: Guard; page: Decimal; offset: Decimal; text: string; next_offset: Decimal | null } }
  | { kind: 'discarded'; data: { guard: Guard } };
export type ExactPart = PartDescriptor & { text: string; bytesValue: Uint8Array };

const DIGEST = /^[0-9a-f]{64}$/;
export const terminalMigration = (value: Snapshot | null) => !!value && (value.phase === 'complete' || value.phase === 'failed');
export const sameGuard = (left: Guard, right: Guard) => left.session === right.session && left.generation === right.generation && left.operation === right.operation;

const documentBytes = (role: InputRole, text: string) => encodeInput(text, role === 'execution_authorization' ? 8 * 1024 * 1024 : 16 * 1024 * 1024);
export async function documentDigest(role: InputRole, text: string): Promise<string> {
  const [{ blake3 }, { bytesToHex }] = await Promise.all([import('@noble/hashes/blake3.js'), import('@noble/hashes/utils.js')]);
  return bytesToHex(blake3(documentBytes(role, text)));
}

export function exactPart(role: InputRole, text: string, blake3: string): ExactPart {
  if (!DIGEST.test(blake3)) throw new Error(`${role.replaceAll('_', ' ')} BLAKE3 must contain 64 lowercase hexadecimal digits.`);
  const bytesValue = documentBytes(role, text);
  if (bytesValue.length === 0) throw new Error(`${role.replaceAll('_', ' ')} document is empty.`);
  return { role, text, bytesValue, bytes: String(bytesValue.length), blake3 };
}

export function operationParts(operation: Operation, input: Partial<Record<InputRole, { text: string; blake3: string }>>): ExactPart[] {
  const roles: InputRole[] = operation.operation === 'run'
    ? ['seal', 'approval', 'policy', ...(input.execution_authorization ? ['execution_authorization' as const] : [])]
    : operation.operation === 'prepare_supplements' ? ['supplement_requests']
      : operation.operation === 'repair_current' || operation.operation === 'repair_keywords' ? ['seal', 'approval', 'repair_request'] : [];
  const parts = roles.map(role => {
    const value = input[role];
    if (!value) throw new Error(`Provide the exact ${role.replaceAll('_', ' ')} document and digest.`);
    return exactPart(role, value.text, value.blake3);
  });
  if (operation.operation === 'run' && parts[1]?.blake3 !== operation.approval_blake3) throw new Error('Run approval digest differs from the uploaded approval document digest.');
  return parts;
}

export function checkedOperation(operation: Operation): Operation {
  const id = (value: string, label: string) => { if (!/^[A-Za-z0-9_-]{1,64}$/.test(value)) throw new Error(`${label} must be the exact 1–64 character opaque identifier.`); return value; };
  if (operation.operation === 'run') return { ...operation, approval_blake3: exactDigest(operation.approval_blake3, 'Approval'), max_steps: decimal(operation.max_steps, 'Maximum steps', 1n), max_seconds: decimal(operation.max_seconds, 'Maximum seconds', 1n), source_open_ms: decimal(operation.source_open_ms, 'Source open milliseconds', 1n), artifact_open_ms: decimal(operation.artifact_open_ms, 'Artifact open milliseconds', 1n), max_artifact_bytes: decimal(operation.max_artifact_bytes, 'Maximum artifact bytes', 1n) };
  if (operation.operation === 'repair_current' || operation.operation === 'repair_keywords') return { ...operation, max_steps: decimal(operation.max_steps, 'Maximum steps', 1n), max_seconds: decimal(operation.max_seconds, 'Maximum seconds', 1n), source_open_ms: decimal(operation.source_open_ms, 'Source open milliseconds', 1n) };
  if (operation.operation === 'status') return { ...operation, run: id(operation.run, 'Run') };
  if (operation.operation === 'repair_status' || operation.operation === 'keyword_repair_status') return { ...operation, repair: id(operation.repair, 'Repair') };
  return operation;
}

export function exactDigest(value: string, label: string): string {
  if (!DIGEST.test(value)) throw new Error(`${label} BLAKE3 must contain 64 lowercase hexadecimal digits.`);
  return value;
}

export function beginRequest(destination: NativePath, catalog: string | null, operation: Operation, parts: ExactPart[], timeout: string, id: string = crypto.randomUUID()): Extract<Request, { action: 'begin' }> {
  if (!/^[A-Za-z0-9_-]{1,64}$/.test(id)) throw new Error('Operation identifier is invalid.');
  return { action: 'begin', operation: id, header: { catalog, destination, operation: checkedOperation(operation), parts: parts.map(({ role, bytes, blake3 }) => ({ role, bytes, blake3 })), timeout_ms: decimal(timeout, 'Operation timeout milliseconds', 1n) } };
}

export async function migration(request: Request): Promise<Response> {
  return command({ command: 'lightroom_migration', args: { request } }, 'lightroom_migration');
}

/** Continue only from the backend-observed byte and role. No optimistic local
 * offset is used, so a lost acknowledgement is reconciled before another write. */
export async function uploadExact(snapshot: Snapshot, parts: ExactPart[], send: (request: Request) => Promise<Response>): Promise<Snapshot> {
  let current = snapshot;
  const completedBytes = (index: number) => parts.slice(0, index).reduce((sum, part) => sum + BigInt(part.bytes), 0n);
  while (current.phase === 'uploading') {
    const index = parts.findIndex(part => part.role === current.next_role);
    if (index < 0) throw new Error(`Backend requested unowned ${current.next_role ?? 'unknown'} input. Cancel or discard it; this UI cannot reconstruct those exact bytes.`);
    const part = parts[index];
    const local = BigInt(decimal(current.uploaded, 'Uploaded bytes')) - completedBytes(index);
    if (local < 0n || local > BigInt(part.bytes)) throw new Error('Backend upload offset differs from the frozen document roster.');
    if (local < BigInt(part.bytes)) {
      const chunk = inputChunk(part.bytesValue, String(local), '16384');
      const reply = await send({ action: 'upload', guard: current.guard, role: part.role, offset: String(local), text: chunk.fragment });
      if (reply.kind !== 'status') throw new Error('Unexpected migration upload response.');
      current = reply.data;
    } else {
      const reply = await send({ action: 'finish', guard: current.guard, role: part.role, blake3: part.blake3 });
      if (reply.kind !== 'status') throw new Error('Unexpected migration finish response.');
      current = reply.data;
    }
  }
  if (current.phase !== 'ready') throw new Error(`Upload stopped in ${current.phase.replaceAll('_', ' ')} state.`);
  return current;
}

export async function readResultPage(snapshot: Snapshot, page: string, send: (request: Request) => Promise<Response>): Promise<string> {
  if (snapshot.phase !== 'complete' || !snapshot.result) throw new Error('A checked complete result is required.');
  const selected = BigInt(decimal(page, 'Result page'));
  if (selected >= BigInt(decimal(snapshot.result.pages, 'Result page count'))) throw new Error('Result page is outside the available range.');
  let offset = '0', result = '';
  for (;;) {
    const reply = await send({ action: 'result_page', guard: snapshot.guard, page, offset, maximum_bytes: '16384' });
    if (reply.kind !== 'page' || !sameGuard(reply.data.guard, snapshot.guard) || reply.data.page !== page || reply.data.offset !== offset) throw new Error('Migration result identity or cursor changed.');
    const bytes = encodeInput(reply.data.text);
    const expected = (BigInt(offset) + BigInt(bytes.length)).toString();
    result += reply.data.text;
    if (reply.data.next_offset === null) return result;
    if (reply.data.next_offset !== expected || bytes.length === 0) throw new Error('Migration result continuation is invalid.');
    offset = reply.data.next_offset;
  }
}

import { invoke, isTauri } from '@tauri-apps/api/core';
import type { Recipe } from './recipe';
import type { OrganizationRequest, OrganizationResponse } from './organization';
import type { MetadataRequest, MetadataResponse } from './metadata';
import type { RelinkRequest, RelinkResponse } from './relink';

export type Decimal = string;
export type NativePath = { encoding: 'UnixBytes' | 'WindowsWide'; units: number[] };
export type VariantKey = { asset_id: string; variant_id: string };
export type Phase = 'closed' | 'opening' | 'indexing' | 'ready' | 'closing' | 'failed';
export type CatalogStatus = { phase: Phase; catalog: string | null; jobs_held: boolean; pending_commands: number; active_previews: number; cancel_requested: boolean; message: string | null };
export type Folder = { id: Decimal; parent: Decimal | null; locator: NativePath; name: string };
export type GridImage = { image_id: string; origin: string; translation_state: string; key: VariantKey; sequence: Decimal; metadata_revision: Decimal; metadata_pending: boolean; state: string; filename: string; rating: Decimal | null; flag: string; label: string; conflicts: string[] };
export type Variant = { key: VariantKey; label: string; revision: Decimal; recipe: Recipe; recipe_digest: string; can_undo: boolean; can_redo: boolean };
export type HistoryEntry = { revision: Decimal; kind: string; recipe: Recipe; recipe_digest: string };
export type PreviewStatus = { ticket: string; key: VariantKey; revision: Decimal; recipe_digest: string; viewport: string; generation: Decimal; state: 'queued' | 'ready' | 'stale' | 'needs_resources' | 'unavailable' | 'failed' | 'cancel_requested' | 'canceled'; message: string | null };
export type CullOperation = { operation: 'rating'; value: number } | { operation: 'flag'; value: 'pick' | 'reject' | 'unflagged' } | { operation: 'label'; value: string };
export type BackupReceipt = { protocol: Decimal; backup_id: string; application_id: Decimal; schema_version: Decimal; database_bytes: Decimal; database_blake3: string };
export type RestoreReceipt = { protocol: Decimal; restore_id: string; backup: BackupReceipt; schema_version: Decimal };
export type BackupStatus = { operation: string; kind: 'create' | 'inspect' | 'restore'; state: 'running' | 'cancel_requested' | 'complete' | 'failed'; cancellation_requested: boolean; progress: { phase: 'Snapshot' | 'Copy' | 'Verify' | 'Hash' | 'Upgrade' | 'Publish'; pages_copied: Decimal; total_pages: Decimal; bytes_processed: Decimal } | null; receipt: { kind: 'backup'; data: BackupReceipt } | { kind: 'restore'; data: RestoreReceipt } | null; error: { message: string; truncated: boolean } | null };
export type SearchOptions = { text?: string | null; keyword?: Decimal | null; keyword_direct?: boolean; folder?: Decimal | null; folder_recursive?: boolean; collection?: string | null; date_from?: string | null; date_until?: string | null; camera_make?: string | null; camera?: string | null; lens?: string | null; format?: string | null; rating?: number | null; flag?: 'pick' | 'reject' | 'unflagged' | null; label?: string | null; only_conflicted?: boolean; sort: 'sequence' | 'capture' | 'filename' | 'rating'; direction: 'ascending' | 'descending' };
export type ImportStatus = { id: string; source: NativePath; phase: 'discovering' | 'draining' | 'complete' | 'cancel_requested' | 'canceled' | 'failed'; imported: Decimal; unchanged: Decimal; failed: Decimal; skipped: Decimal; metadata_updated: Decimal; metadata_warnings: Decimal; awaiting_resources: Decimal; pending_previews: number; error: string | null; error_source: NativePath | null };
type AtCatalog = { catalog: string };
type AtVariant = AtCatalog & { key: VariantKey };
type AtRevision = AtVariant & { expected_revision: Decimal };
export type Request =
  | { command: 'relink'; args: AtCatalog & { request: RelinkRequest } }
  | { command: 'metadata'; args: AtCatalog & { request: MetadataRequest } }
  | { command: 'organization'; args: AtCatalog & { request: OrganizationRequest } }
  | { command: 'status' | 'backup_status' }
  | { command: 'backup_create'; args: AtCatalog & { bundle: NativePath } }
  | { command: 'backup_inspect'; args: { bundle: NativePath } }
  | { command: 'backup_restore'; args: { bundle: NativePath; destination: NativePath } }
  | { command: 'backup_cancel'; args: { operation: string } }
  | { command: 'restore_status'; args: AtCatalog }
  | { command: 'resume_restored_jobs'; args: AtCatalog & { restore_id: string; acknowledge_pending_jobs: boolean } }
  | { command: 'open_existing' | 'create'; args: { path: NativePath } }
  | { command: 'close' | 'import_status'; args: AtCatalog }
  | { command: 'import_start' | 'import_resume'; args: AtCatalog & { source: NativePath } }
  | { command: 'import_cancel'; args: AtCatalog & { import: string } }
  | { command: 'folders'; args: AtCatalog & { parent: Decimal | null; after: Decimal; limit: number } }
  | { command: 'images'; args: AtCatalog & { folder: Decimal | null; recursive: boolean; text: string | null; cursor: string | null; limit: number } }
  | { command: 'search'; args: AtCatalog & { options: SearchOptions; cursor: string | null; limit: number } }
  | { command: 'variant' | 'image'; args: AtVariant }
  | { command: 'variants'; args: AtCatalog & { asset_id: string; after: Decimal; limit: number } }
  | { command: 'create_variant'; args: AtRevision & { label: string } }
  | { command: 'save_recipe'; args: AtRevision & { recipe: Recipe } }
  | { command: 'undo' | 'redo'; args: AtRevision }
  | { command: 'history'; args: AtVariant & { after: Decimal; limit: number } }
  | { command: 'cull'; args: AtRevision & { operation: CullOperation } }
  | { command: 'preview'; args: AtVariant & { tier: 'thumbnail' | 'large'; interactive: boolean; viewport: string; generation: Decimal; foreground: boolean } }
  | { command: 'release_viewport'; args: AtCatalog & { viewport: string; generation: Decimal } }
  | { command: 'preview_status' | 'cancel_preview'; args: AtCatalog & { ticket: string } };
export interface Data {
  relink: RelinkResponse;
  metadata: MetadataResponse;
  organization: OrganizationResponse;
  backup: BackupStatus | null;
  restore: { receipt: RestoreReceipt; jobs_held: boolean } | null;
  import: ImportStatus | null;
  status: CatalogStatus;
  folders: { rows: Folder[]; next: Decimal | null };
  images: { rows: GridImage[]; next: string | null; has_more: boolean; page_complete: boolean; scanned: number };
  variant: Variant;
  image: GridImage;
  variants: { rows: [Decimal, Variant][]; next: Decimal | null };
  history: { rows: HistoryEntry[]; next: Decimal | null };
  culled: { metadata_revision: Decimal };
  preview: PreviewStatus;
}
type Response = { [K in keyof Data]: { kind: K; data: Data[K] } }[keyof Data];
type Reply = { status: 'ok'; value: Response } | { status: 'error'; error: { code: string; message: string } };

export class CatalogError extends Error {
  readonly code: string;
  constructor(code: string, message: string) { super(message); this.name = 'CatalogError'; this.code = code; }
}

export const desktopAvailable = isTauri();
export const imageKey = (key: VariantKey) => JSON.stringify([key.asset_id, key.variant_id]);

export async function command<K extends keyof Data>(request: Request, kind: K, signal?: AbortSignal): Promise<Data[K]> {
  if (!desktopAvailable) throw new CatalogError('desktop_required', 'Open the PhotoCatalog desktop app to access your catalog.');
  if (signal?.aborted) throw new DOMException('Canceled', 'AbortError');
  const operation = crypto.randomUUID();
  const cancel = () => { void invoke('catalog_cancel_operation', { operation }).catch(() => {}); };
  signal?.addEventListener('abort', cancel, { once: true });
  try {
    const reply = await invoke<Reply>('catalog_command', { operation, request });
    if (signal?.aborted) throw new DOMException('Canceled', 'AbortError');
    if (reply.status === 'error') throw new CatalogError(reply.error.code, reply.error.message);
    if (reply.value.kind !== kind) throw new CatalogError('protocol', `Unexpected ${reply.value.kind} response to ${request.command}.`);
    return reply.value.data as Data[K];
  } finally { signal?.removeEventListener('abort', cancel); }
}

export async function chooseFolder(createCatalog = false): Promise<{ path: NativePath; display: string } | null> {
  return invoke('catalog_choose_folder', { createCatalog });
}

export async function chooseSource(): Promise<{ path: NativePath; display: string } | null> { return chooseLocation('originals'); }

export async function chooseLocation(purpose: 'originals' | 'backup_bundle' | 'new_backup' | 'new_restore'): Promise<{ path: NativePath; display: string } | null> { return invoke('catalog_choose_location', { purpose }); }

export async function previewBlob(catalog: string, ticket: string): Promise<Blob> {
  const handoff = crypto.randomUUID();
  try {
    const bytes = await invoke<ArrayBuffer>('catalog_preview_bytes', { catalog, ticket, handoff });
    return new Blob([bytes], { type: 'image/jpeg' });
  } finally { await invoke('catalog_preview_release', { handoff }); }
}

// Display only: never reconstruct filesystem authority from this text.
export function displayPath(path: NativePath): string {
  if (path.encoding === 'UnixBytes') return new TextDecoder().decode(new Uint8Array(path.units));
  let text = ''; for (let offset = 0; offset < path.units.length; offset += 1024) text += String.fromCharCode(...path.units.slice(offset, offset + 1024));
  return text;
}

export function errorText(error: unknown): string { return error instanceof Error ? error.message : String(error); }

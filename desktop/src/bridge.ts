import { invoke, isTauri } from '@tauri-apps/api/core';
import type { Recipe } from './recipe';

export type Decimal = string;
export type NativePath = { encoding: 'UnixBytes' | 'WindowsWide'; units: number[] };
export type VariantKey = { asset_id: string; variant_id: string };
export type Phase = 'closed' | 'opening' | 'indexing' | 'ready' | 'closing' | 'failed';
export type CatalogStatus = { phase: Phase; catalog: string | null; jobs_held: boolean; pending_commands: number; active_previews: number; cancel_requested: boolean; message: string | null };
export type Folder = { id: Decimal; parent: Decimal | null; locator: NativePath; name: string };
export type GridImage = { image_id: string; key: VariantKey; sequence: Decimal; metadata_revision: Decimal; metadata_pending: boolean; state: string; filename: string; rating: Decimal | null; flag: string; label: string; conflicts: string[] };
export type Variant = { key: VariantKey; label: string; revision: Decimal; recipe: Recipe; recipe_digest: string; can_undo: boolean; can_redo: boolean };
export type HistoryEntry = { revision: Decimal; kind: string; recipe: Recipe; recipe_digest: string };
export type PreviewStatus = { ticket: string; key: VariantKey; revision: Decimal; recipe_digest: string; viewport: string; generation: Decimal; state: 'queued' | 'ready' | 'stale' | 'needs_resources' | 'unavailable' | 'failed' | 'cancel_requested' | 'canceled'; message: string | null };
export type CullOperation = { operation: 'rating'; value: number } | { operation: 'flag'; value: 'pick' | 'reject' | 'unflagged' } | { operation: 'label'; value: string };
type AtCatalog = { catalog: string };
type AtVariant = AtCatalog & { key: VariantKey };
type AtRevision = AtVariant & { expected_revision: Decimal };
export type Request =
  | { command: 'status' }
  | { command: 'open_existing' | 'create'; args: { path: NativePath } }
  | { command: 'close'; args: AtCatalog }
  | { command: 'folders'; args: AtCatalog & { parent: Decimal | null; after: Decimal; limit: number } }
  | { command: 'images'; args: AtCatalog & { folder: Decimal | null; recursive: boolean; text: string | null; cursor: string | null; limit: number } }
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

export async function previewBlob(catalog: string, ticket: string): Promise<Blob> {
  const handoff = crypto.randomUUID();
  try {
    const bytes = await invoke<ArrayBuffer>('catalog_preview_bytes', { catalog, ticket, handoff });
    return new Blob([bytes], { type: 'image/jpeg' });
  } finally { await invoke('catalog_preview_release', { handoff }); }
}

export function errorText(error: unknown): string { return error instanceof Error ? error.message : String(error); }

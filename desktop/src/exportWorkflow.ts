import type { Budgets, ExecutionLimits, Naming, Options } from './photoExport';

export const U64_MAX = '18446744073709551615';
export function decimal(value: string, label: string, min = '1', max = U64_MAX): string {
  if (!/^(0|[1-9][0-9]*)$/.test(value) || value.length > 20 || BigInt(value) < BigInt(min) || BigInt(value) > BigInt(max)) throw new Error(`${label} must be a canonical whole number from ${min} to ${max}.`);
  return value;
}
export function naming(prefix: string, suffix: string, variant_suffix: boolean, sequence: string, count: number): Naming {
  for (const [label, value] of [['Prefix', prefix], ['Suffix', suffix]]) {
    if (new TextEncoder().encode(value).length > 128 || /[\\/\x00-\x1f\x7f]/.test(value)) throw new Error(`${label} must fit 128 UTF-8 bytes and contain no path separators or control characters.`);
  }
  const sequence_start = sequence === '' ? null : decimal(sequence, 'Starting sequence', '0');
  if (sequence_start !== null && BigInt(sequence_start) + BigInt(Math.max(0, count - 1)) > BigInt(U64_MAX)) throw new Error('The filename sequence would exceed u64.');
  return { prefix, suffix, variant_suffix, sequence_start };
}
export const executionFields = [
  ['worker', 'Worker live bytes'], ['working', 'Shared working bytes'],
  ['decodeEncoded', 'Decode encoded bytes'], ['decodePixels', 'Decode intermediate pixels'], ['decodeAllocation', 'Decode allocation bytes'],
  ['renderPixels', 'Render pixels'], ['renderAllocation', 'Render allocation bytes'], ['renderLive', 'Render live bytes'],
  ['encodePixels', 'Encode pixels'], ['encodeAllocation', 'Encode allocation bytes'], ['encodeLive', 'Encode live bytes'],
  ['metadata', 'Encoded metadata bytes'], ['rowBuffer', 'Encode row buffer bytes'], ['extent', 'Encoded output bytes'],
] as const;
export type ExecutionDraft = Record<typeof executionFields[number][0], string>;
export function executionDraft(v: ExecutionLimits): ExecutionDraft {
  return { worker: v.worker_bytes, working: v.working_bytes, decodeEncoded: v.render.decode.max_encoded_bytes, decodePixels: v.render.decode.max_intermediate_pixels, decodeAllocation: v.render.decode.max_allocation_bytes, renderPixels: v.render.render.max_pixels, renderAllocation: v.render.render.max_allocation_bytes, renderLive: v.render.render.max_live_bytes, encodePixels: v.render.encode.render.max_pixels, encodeAllocation: v.render.encode.render.max_allocation_bytes, encodeLive: v.render.encode.render.max_live_bytes, metadata: v.render.encode.max_metadata_bytes, rowBuffer: v.render.encode.row_buffer_bytes, extent: v.render.max_encoded_extent };
}
export function execution(v: ExecutionDraft, options: Options): ExecutionLimits {
  for (const [key, label] of executionFields) decimal(v[key], label);
  decimal(v.working, 'Shared working bytes', '1', options.execution.working_bytes);
  if (BigInt(v.worker) > BigInt(v.working)) throw new Error('Worker bytes cannot exceed shared working bytes.');
  for (const key of ['decodeAllocation', 'renderLive', 'encodeLive', 'rowBuffer'] as const) decimal(v[key], executionFields.find(([field]) => field === key)![1], '1', v.worker);
  decimal(v.renderPixels, 'Render pixels', '1', '100000000'); decimal(v.encodePixels, 'Encode pixels', '1', '100000000'); decimal(v.metadata, 'Encoded metadata bytes', '1', '16777216');
  return { worker_bytes: v.worker, working_bytes: v.working, render: {
    decode: { max_encoded_bytes: v.decodeEncoded, max_intermediate_pixels: v.decodePixels, max_allocation_bytes: v.decodeAllocation },
    render: { max_pixels: v.renderPixels, max_allocation_bytes: v.renderAllocation, max_live_bytes: v.renderLive },
    encode: { render: { max_pixels: v.encodePixels, max_allocation_bytes: v.encodeAllocation, max_live_bytes: v.encodeLive }, max_metadata_bytes: v.metadata, row_buffer_bytes: v.rowBuffer }, max_encoded_extent: v.extent,
  } };
}
export function budgets(v: Budgets): Budgets {
  decimal(v.max_original_bytes, 'Maximum existing destination bytes'); decimal(v.max_payload_bytes, 'Maximum payload bytes');
  decimal(v.alias_limits.directories, 'Alias directories', '0', '65536'); decimal(v.alias_limits.candidates, 'Alias candidates', '1', '4096'); return v;
}

/** Stop local waiting without claiming the underlying IPC request was canceled. */
export function waitForExport<T>(pending: Promise<T>, signal: AbortSignal): Promise<T> {
  return new Promise((resolve, reject) => {
    const stop = () => reject(new Error('Stopped waiting. The request may still be pending; inspect saved work and operation status.'));
    if (signal.aborted) stop(); else signal.addEventListener('abort', stop, { once: true });
    void pending.then(resolve, reject).finally(() => signal.removeEventListener('abort', stop));
  });
}

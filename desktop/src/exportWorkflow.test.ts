import { describe, expect, it } from 'vitest';
import { waitForExport, budgets, decimal, execution, executionDraft, naming, U64_MAX } from './exportWorkflow';
import type { ExecutionLimits, Options } from './photoExport';
const limits: ExecutionLimits = {
  worker_bytes: '1024', working_bytes: '2048', render: {
    decode: { max_encoded_bytes: U64_MAX, max_intermediate_pixels: '9007199254740993', max_allocation_bytes: '512' },
    render: { max_pixels: '100000000', max_allocation_bytes: '1024', max_live_bytes: '1024' },
    encode: { render: { max_pixels: '100000000', max_allocation_bytes: '1024', max_live_bytes: '1024' }, max_metadata_bytes: '128', row_buffer_bytes: '64' }, max_encoded_extent: U64_MAX,
  },
};
const options = { execution: limits } as Options;
describe('explicit export authority input', () => {
  it('preserves full u64 values and rejects ambiguous lexical inputs', () => {
    expect(decimal('9007199254740993', 'Bytes')).toBe('9007199254740993');
    expect(decimal(U64_MAX, 'Bytes')).toBe(U64_MAX);
    for (const value of ['01', '+1', '-1', '1.0', '1e3', ' 1', '', '18446744073709551616']) expect(() => decimal(value, 'Bytes')).toThrow();
  });
  it('maps every execution field without changing any decimal', () => {
    expect(execution(executionDraft(limits), options)).toEqual(limits);
  });
  it('enforces the actual host/core consistency rules without reducing other u64 ceilings', () => {
    const draft = executionDraft(limits);
    for (const [key, value] of Object.entries({ working: '2049', worker: '2049', decodeAllocation: '1025', renderLive: '1025', encodeLive: '1025', rowBuffer: '1025', renderPixels: '100000001', encodePixels: '100000001', metadata: '16777217', extent: '0' })) expect(() => execution({ ...draft, [key]: value }, options)).toThrow();
    expect(execution({ ...draft, renderAllocation: U64_MAX, encodeAllocation: U64_MAX }, options).render.render.max_allocation_bytes).toBe(U64_MAX);
  });
  it('allows zero alias directories but preserves positive candidate and payload bounds', () => {
    const input = { max_original_bytes: U64_MAX, max_payload_bytes: U64_MAX, alias_limits: { directories: '0', candidates: '4096' } };
    expect(budgets(input)).toEqual(input);
    expect(() => budgets({ ...input, alias_limits: { directories: '65537', candidates: '1' } })).toThrow();
    expect(() => budgets({ ...input, alias_limits: { directories: '0', candidates: '0' } })).toThrow();
  });
  it('validates native naming components and exact sequence overflow', () => {
    expect(naming('é', '-edited', true, '9007199254740993', 100).sequence_start).toBe('9007199254740993');
    expect(naming('', '', false, '', 1).sequence_start).toBeNull();
    expect(naming('', '', false, U64_MAX, 1).sequence_start).toBe(U64_MAX);
    expect(() => naming('', '', false, U64_MAX, 2)).toThrow();
    for (const prefix of ['../x', 'a\\b', 'a\u0000b', 'é'.repeat(65)]) expect(() => naming(prefix, '', true, '', 1)).toThrow();
  });
});

describe('local export waiting', () => {
  it('releases an abandoned wait while retaining independent acknowledgement custody', async () => {
    let acknowledge!: (value: string) => void;
    const ipc = new Promise<string>(resolve => { acknowledge = resolve; });
    let settled = false; void ipc.then(() => { settled = true; });
    const abort = new AbortController(); const waiting = waitForExport(ipc, abort.signal);
    abort.abort(); await expect(waiting).rejects.toThrow('may still be pending');
    expect(settled).toBe(false);
    acknowledge('exact owned reply'); await ipc;
    expect(settled).toBe(true);
    await expect(waiting).rejects.toThrow('Stopped waiting');
  });
  it('consumes late transport errors after local cancellation without replacing the result', async () => {
    let fail!: (e: Error) => void;
    const ipc = new Promise<string>((_, reject) => { fail = reject; });
    const abort = new AbortController(); abort.abort();
    const waiting = waitForExport(ipc, abort.signal);
    await expect(waiting).rejects.toThrow('Stopped waiting');
    fail(new Error('Late transport error'));
    await expect(waiting).rejects.toThrow('Stopped waiting');
  });
});

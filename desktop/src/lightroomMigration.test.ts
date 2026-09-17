import { describe, expect, it } from 'vitest';
import { beginRequest, documentDigest, exactPart, operationParts, readResultPage, uploadExact, type Operation, type Request, type Response, type Snapshot } from './lightroomMigration';

const digest = (letter: string) => letter.repeat(64);
const part = async (role: Parameters<typeof documentDigest>[0], text: string) => ({ text, blake3: await documentDigest(role, text) });
const guard = { session: 'session', generation: 'generation', operation: 'operation' };
const path = { encoding: 'UnixBytes' as const, units: [47, 116, 109, 112] };
const run = (): Extract<Operation, { operation: 'run' }> => ({ operation: 'run', approval_blake3: digest('b'), max_steps: '9007199254740993', max_seconds: '3600', source_open_ms: '30000', artifact_open_ms: '30000', max_artifact_bytes: '1073741824' });

describe('Lightroom migration protocol', () => {
  it('builds the exact ordered run roster without coercing u64 limits or UTF-8 bytes', async () => {
    const documents = { seal: await part('seal', '{"seal":"é"}'), approval: await part('approval', '{"approval":true}'), policy: await part('policy', '{"policy":1}'), execution_authorization: await part('execution_authorization', '{"authorized":"🌲"}') };
    const operation = { ...run(), approval_blake3: documents.approval.blake3 };
    const parts = operationParts(operation, documents);
    expect(parts.map(part => part.role)).toEqual(['seal', 'approval', 'policy', 'execution_authorization']);
    expect(parts[0].bytes).toBe(String(new TextEncoder().encode(documents.seal.text).length));
    const request = beginRequest(path, 'opaque-catalog', operation, parts, '3900000', 'stable-operation');
    expect(request.header.operation).toMatchObject({ max_steps: '9007199254740993' });
    expect(request.header.parts).not.toHaveProperty('0.text');
  });

  it('rejects digest substitution, ambiguous decimals, and lossy UTF-16', async () => {
    const docs = { seal: await part('seal', '{}'), approval: await part('approval', '{}'), policy: await part('policy', '{}') };
    expect(() => operationParts(run(), docs)).toThrow(/approval digest differs/i);
    const invalid = run(); invalid.max_steps = '01';
    expect(() => beginRequest(path, null, invalid, [], '1')).toThrow(/whole decimal/i);
    expect(() => exactPart('seal', '\ud800', digest('a'))).toThrow(/surrogate/i);
    expect(await documentDigest('seal', 'abc')).toBe('6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85');
  });

  it('covers every backend operation and its exact document roster', async () => {
    const docs = { seal: await part('seal', '{}'), approval: await part('approval', '{}'), policy: await part('policy', '{}'), repair_request: await part('repair_request', '{}'), supplement_requests: await part('supplement_requests', '[]') };
    const runOperation = { ...run(), approval_blake3: docs.approval.blake3 };
    const operations: [Operation, string[]][] = [
      [runOperation, ['seal', 'approval', 'policy']],
      [{ operation: 'status', run: 'run_1' }, []],
      [{ operation: 'prepare_supplements' }, ['supplement_requests']],
      [{ operation: 'repair_current', max_steps: '1', max_seconds: '1', source_open_ms: '1' }, ['seal', 'approval', 'repair_request']],
      [{ operation: 'repair_status', repair: 'repair_1' }, []],
      [{ operation: 'repair_keywords', max_steps: '1', max_seconds: '1', source_open_ms: '1' }, ['seal', 'approval', 'repair_request']],
      [{ operation: 'keyword_repair_status', repair: 'repair_2' }, []],
    ];
    for (const [operation, roles] of operations) {
      const parts = operationParts(operation, docs);
      expect(parts.map(part => part.role)).toEqual(roles);
      expect(beginRequest(path, operation.operation.includes('repair') || operation.operation === 'status' ? 'catalog' : null, operation, parts, '10').header.operation.operation).toBe(operation.operation);
    }
  });

  it('continues multipart upload only from backend-observed roles and byte offsets', async () => {
    const text = 'é'.repeat(9000);
    const parts = [exactPart('supplement_requests', text, await documentDigest('supplement_requests', text))];
    let uploaded = 0n, finished = false;
    const sent: Request[] = [];
    const snapshot = (): Snapshot => ({ guard, phase: finished ? 'ready' : 'uploading', catalog: null, uploaded: String(uploaded), next_role: finished ? null : 'supplement_requests', progress: null, failure: null, result: null });
    const send = async (request: Request): Promise<Response> => {
      sent.push(request);
      if (request.action === 'upload') uploaded += BigInt(new TextEncoder().encode(request.text).length);
      else if (request.action === 'finish') finished = true;
      return { kind: 'status', data: snapshot() };
    };
    const complete = await uploadExact(snapshot(), parts, send);
    expect(complete.phase).toBe('ready');
    expect(sent.filter(value => value.action === 'upload').length).toBe(2);
    expect(sent.at(-1)?.action).toBe('finish');
  });

  it('assembles one result page from authoritative UTF-8 cursors', async () => {
    const value: Snapshot = { guard, phase: 'complete', catalog: null, uploaded: '0', next_role: null, progress: null, failure: null, result: { bytes: '7', pages: '2', blake3: digest('f') } };
    const replies: Response[] = [
      { kind: 'page', data: { guard, page: '1', offset: '0', text: 'é', next_offset: '2' } },
      { kind: 'page', data: { guard, page: '1', offset: '2', text: '{}', next_offset: null } },
    ];
    expect(await readResultPage(value, '1', async () => replies.shift()!)).toBe('é{}');
    await expect(readResultPage(value, '2', async () => { throw new Error('unreachable'); })).rejects.toThrow(/outside/i);
  });
});

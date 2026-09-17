import { describe, expect, it } from 'vitest';
import { metadataWriteOperationError, type NativePath, type Operation, type RecoveryEntry, type SidecarPlan, type SidecarReceipt } from './metadataWrite';

const path: NativePath = { encoding: 'UnixBytes', units: [47, 116, 109, 112, 47, 112, 104, 111, 116, 111, 46, 120, 109, 112] };
const identity = {
  image_id: 'image-1',
  key: { asset_id: 'asset-1', variant_id: 'master' },
  metadata_revision: 4,
  pixel_generation: 1,
  shared_source_epoch: 2,
  physical_generation: 3,
};

describe('metadata_write frontend DTO contract', () => {
  it('surfaces a terminal operation error string verbatim', () => {
    const operation: Operation = {
      id: 'operation', attempt: 'attempt', request_digest: 'a'.repeat(64), epoch: '2', kind: 'sidecar_apply',
      phase: 'failed', stage: 'draining', cancel_requested: false, progress: '0', result: null,
      error: 'resolve metadata conflicts before export',
    };
    expect(metadataWriteOperationError(operation)).toBe('resolve metadata conflicts before export');
    expect(metadataWriteOperationError({ ...operation, error: null })).toBe('Metadata operation failed.');
  });

  it('matches sidecar plan, publication receipt, and recovery entry payloads', () => {
    const receipt: SidecarReceipt = {
      version: 2, state: 'Published', destination: path, recovery_directory: path,
      captured_original: null, detail: 'metadata sidecar published',
    };
    const plan: SidecarPlan = {
      row: '7', operation: 'operation', version: '3', owner: { kind: 'image', identity }, revision: '4',
      base_model: '12', destination: path, expected: null, existing: null, max_existing_bytes: '16777216',
      alias_limits: { directories: '4096', candidates: '256' }, payload_bytes: '512',
      payload_digest: 'b'.repeat(64), authority_blake3: 'c'.repeat(64), current: true, receipt,
    };
    const recovery: RecoveryEntry = {
      directory: path, name: path, kind: 'known', operation: plan.operation,
      plan_digest: plan.authority_blake3, detail: 'ready for explicit recovery',
    };
    expect(plan.receipt?.version).toBe(2);
    expect(recovery.name).toEqual(path);
    expect(typeof recovery.detail).toBe('string');
  });
});

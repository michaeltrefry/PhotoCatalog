import { describe, expect, it } from 'vitest';
import type { MetadataModel, Observation, RetainedText } from './metadata';
import { selectMetadataModel } from './metadataModelSelection';

const text = (inline: string): RetainedText => ({ bytes: String(inline.length), inline, reference: { kind: 'model', model: '41', field: 'descriptor' } });
const observation: Pick<Observation, 'id' | 'source' | 'current'> = { id: '17', source: '9', current: false };
const model = (error: RetainedText | null): MetadataModel => ({ id: '41', ordinal: '2', blob_hash: 'a'.repeat(64), bytes: '812', descriptor: text('complete'), projection: text('{}'), error });

describe('retained metadata model selection', () => {
  it('carries exact model and source identity only after an explicit complete-row selection', () => {
    expect(selectMetadataModel(model(null), observation)).toEqual({ id: '41', ordinal: '2', blobHash: 'a'.repeat(64), bytes: '812', observation: '17', source: '9', currentObservation: false });
  });

  it('refuses an error model instead of silently making it usable', () => {
    expect(() => selectMetadataModel(model(text('parse failed')), observation)).toThrow(/parse error/);
  });
});

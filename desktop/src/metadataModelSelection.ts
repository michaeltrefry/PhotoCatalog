import type { MetadataModel, Observation } from './metadata';

export type MetadataModelDestination = 'edit' | 'sidecar';
export type SelectedMetadataModel = {
  id: string;
  ordinal: string;
  blobHash: string;
  bytes: string;
  observation: string;
  source: string;
  currentObservation: boolean;
};

/** Selection is always an explicit action on one complete retained model row. */
export function selectMetadataModel(model: MetadataModel, observation: Pick<Observation, 'id' | 'source' | 'current'>): SelectedMetadataModel {
  if (model.error !== null) throw new Error('A retained model with a parse error cannot be used as an editing or sidecar base.');
  return {
    id: model.id,
    ordinal: model.ordinal,
    blobHash: model.blob_hash,
    bytes: model.bytes,
    observation: observation.id,
    source: observation.source,
    currentObservation: observation.current,
  };
}

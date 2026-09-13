import { expect, it } from 'vitest';
import { imageKey, type GridImage } from './bridge';
import { chosenCopyTargets, copyCandidates } from './copySelection';

const row = (asset: string, variant = 'master') => ({ filename: 'same.CR2', key: { asset_id: asset, variant_id: variant } }) as GridImage;

it('excludes only the frozen source and duplicate keys, keeping sibling variants distinct', () => {
  const source = row('a'), sibling = row('a', '9007199254740993'), target = row('b');
  expect(copyCandidates([source, sibling, target, target], source.key)).toEqual([sibling, target]);
});

it('does not append a stale selection from a different page or confuse matching filenames', () => {
  const old = row('old'), current = row('current'), unselected = row('third');
  const chosen = new Set([imageKey(old.key), imageKey(current.key)]);
  expect(chosenCopyTargets([current, unselected], chosen, old.key)).toEqual([current]);
  expect(chosen.size).toBe(2);
});

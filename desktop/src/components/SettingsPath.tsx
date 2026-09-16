import { useState } from 'react';
import { settingsPath, type SettingsStep } from '../metadata';
import { ErrorNotice } from './Controls';

export function SettingsPath({ onChange }: { onChange: (path: string) => void }) {
  const [steps, setSteps] = useState<SettingsStep[]>([]); const [kind, setKind] = useState<SettingsStep['kind']>('name');
  const [name, setName] = useState(''); const [index, setIndex] = useState('0'); const [namespace, setNamespace] = useState(''); const [error, setError] = useState('');
  const update = (next: SettingsStep[]) => { try { const encoded = settingsPath(next); setSteps(next); onChange(encoded); setError(''); } catch (e) { setError(e instanceof Error ? e.message : String(e)); } };
  return <div><p>Settings location: {steps.length ? steps.map(step => step.kind === 'name' ? step.name : step.kind === 'index' ? `[${step.index}]` : `${step.name} [${step.ordinal}]`).join(' → ') : 'Top level'}</p>
    <button disabled={!steps.length} onClick={() => update(steps.slice(0, -1))}>Remove last step</button><button disabled={!steps.length} onClick={() => update([])}>Top level</button>
    <details><summary>Choose a nested settings location</summary><label className="form-field">Step type<select value={kind} onChange={e => setKind(e.target.value as SettingsStep['kind'])}><option value="name">Named property</option><option value="index">Array index</option><option value="xml">XML element</option></select></label>
      {kind !== 'index' && <label className="form-field">Property or element name<input value={name} maxLength={1024} onChange={e => setName(e.target.value)} /></label>}
      {kind === 'xml' && <label className="form-field">XML namespace<input value={namespace} maxLength={1024} onChange={e => setNamespace(e.target.value)} /></label>}
      {kind !== 'name' && <label className="form-field">{kind === 'xml' ? 'Element occurrence (starting at 0)' : 'Array index (starting at 0)'}<input inputMode="numeric" value={index} maxLength={20} onChange={e => setIndex(e.target.value)} /></label>}
      {error && <ErrorNotice message={error} />}<button disabled={steps.length >= 16 || kind !== 'index' && !name} onClick={() => update([...steps, kind === 'name' ? { kind, name } : kind === 'index' ? { kind, index } : { kind, name, namespace, ordinal: index }])}>Add step</button>
    </details>
  </div>;
}

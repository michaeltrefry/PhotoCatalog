import type { Recipe } from '../recipe';
import { Section, Slider } from './Controls';

export function RecipeControls({ recipe, onChange, disabled }: { recipe: Recipe; onChange: (recipe: Recipe) => void; disabled?: boolean }) {
  const s = recipe.settings;
  const patch = (settings: Partial<Recipe['settings']>) => onChange({ ...recipe, settings: { ...s, ...settings } });
  const wb = s.white_balance;
  return <>
    <Section title="Light">
      <Slider label="Exposure" value={s.exposure_ev} min={-10} max={10} step={0.05} unit="EV" disabled={disabled} onChange={exposure_ev => patch({ exposure_ev })} />
      {(['contrast', 'highlights', 'shadows'] as const).map(k => <Slider key={k} label={k[0].toUpperCase() + k.slice(1)} value={Math.round(s[k] * 100)} min={-100} max={100} step={1} disabled={disabled} onChange={v => patch({ [k]: v / 100 })} />)}
    </Section>
    <Section title="Color">
      <label className="form-field">White balance<select value={wb.mode} disabled={disabled} onChange={e => patch({ white_balance: e.target.value === 'as_shot' ? { mode: 'as_shot' } : { mode: 'temperature_tint', kelvin: 6500, tint: 0 } })}>
        <option value="as_shot">As shot</option><option value="temperature_tint">Temperature and tint</option>
      </select></label>
      {wb.mode === 'temperature_tint' && <>
        <Slider label="Temperature" value={wb.kelvin} min={2000} max={50000} step={50} unit="Kelvin" disabled={disabled} onChange={kelvin => patch({ white_balance: { ...wb, kelvin } })} />
        <Slider label="Tint" value={wb.tint} min={-150} max={150} step={1} disabled={disabled} onChange={tint => patch({ white_balance: { ...wb, tint } })} />
      </>}
      {(['vibrance', 'saturation'] as const).map(k => <Slider key={k} label={k[0].toUpperCase() + k.slice(1)} value={Math.round(s[k] * 100)} min={-100} max={100} step={1} disabled={disabled} onChange={v => patch({ [k]: v / 100 })} />)}
    </Section>
    <Section title="Crop & straighten">
      <Slider label="Straighten" value={s.straighten_degrees} min={-45} max={45} step={0.1} unit="degrees" disabled={disabled} onChange={straighten_degrees => patch({ straighten_degrees })} />
      <label className="checkbox"><input type="checkbox" checked={s.crop !== null} disabled={disabled} onChange={e => patch({ crop: e.target.checked ? { left: 0, top: 0, right: 1, bottom: 1 } : null })} />Crop image</label>
      {s.crop && <div className="crop-fields">{(['left', 'top', 'right', 'bottom'] as const).map(k => <label key={k}>{k}<input aria-label={`Crop ${k} percent`} type="number" min="0" max="100" step="0.1" disabled={disabled} value={Number((s.crop![k] * 100).toFixed(2))} onChange={e => { const v = e.currentTarget.valueAsNumber; if (Number.isFinite(v)) patch({ crop: { ...s.crop!, [k]: v / 100 } }); }} /></label>)}</div>}
      <p className="hint">Crop edges use the oriented original. Straightening can expose transparent corners.</p>
    </Section>
    <Section title="Detail" open={false}>
      <Slider label="Sharpening" value={Math.round(s.sharpening.amount * 100)} min={0} max={200} step={1} disabled={disabled} onChange={v => patch({ sharpening: { ...s.sharpening, amount: v / 100 } })} />
      <Slider label="Radius" value={s.sharpening.radius_px} min={0.1} max={10} step={0.1} unit="pixels" disabled={disabled} onChange={radius_px => patch({ sharpening: { ...s.sharpening, radius_px } })} />
      <Slider label="Luminance noise" value={Math.round(s.noise_reduction.luminance * 100)} min={0} max={100} step={1} disabled={disabled} onChange={v => patch({ noise_reduction: { ...s.noise_reduction, luminance: v / 100 } })} />
      <Slider label="Color noise" value={Math.round(s.noise_reduction.chroma * 100)} min={0} max={100} step={1} disabled={disabled} onChange={v => patch({ noise_reduction: { ...s.noise_reduction, chroma: v / 100 } })} />
    </Section>
  </>;
}

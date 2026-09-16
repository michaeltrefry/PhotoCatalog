import type { ExportSettings as Settings } from '../exportSettings';
import { ErrorNotice } from './Controls';
import './export.css';

export function ExportSettings({ value, onChange, disabled, choosingProfile, chooseProfile, error }: {
  value: Settings; onChange: (value: Settings) => void; disabled: boolean;
  choosingProfile: boolean; chooseProfile: () => void; error?: string;
}) {
  const change = <K extends keyof Settings>(key: K, next: Settings[K]) => onChange({ ...value, [key]: next });
  const float = value.format === 'tiff' && value.depth === 'float32';
  return <section className="export-settings" aria-label="Photo export settings">
    <fieldset disabled={disabled}><legend>File format</legend>
      <label>Format<select value={value.format} onChange={e => change('format', e.target.value as Settings['format'])}><option value="jpeg">JPEG</option><option value="png">PNG</option><option value="tiff">TIFF</option></select></label>
      {value.format === 'jpeg' ? <label>JPEG quality<input type="number" min="1" max="100" step="1" value={value.quality} onChange={e => change('quality', e.target.value)} /></label> : <label>Bit depth<select value={value.depth} onChange={e => change('depth', e.target.value as Settings['depth'])}><option value="eight">8-bit</option><option value="sixteen">16-bit</option>{(value.format === 'tiff' || value.depth === 'float32') && <option value="float32" disabled={value.format !== 'tiff'}>32-bit float (TIFF)</option>}</select></label>}
      <p className="hint">{float ? 'Float output preserves signed and extended color values.' : 'Integer output clips color values to the 0–1 range.'}</p>
    </fieldset>
    <fieldset disabled={disabled}><legend>Image size</legend>
      <label>Dimensions<select value={value.size} onChange={e => change('size', e.target.value as Settings['size'])}><option value="original">Original edited dimensions</option><option value="fit">Fit inside a bounding box</option></select></label>
      {value.size === 'fit' && <><label>Maximum width in pixels<input type="number" min="1" max="40000" step="1" value={value.width} onChange={e => change('width', e.target.value)} /></label><label>Maximum height in pixels<input type="number" min="1" max="40000" step="1" value={value.height} onChange={e => change('height', e.target.value)} /></label><label className="checkbox"><input type="checkbox" checked={value.upscale} onChange={e => change('upscale', e.target.checked)} />Allow enlargement</label><p className="hint">Aspect ratio is preserved. Final output is limited to 100 megapixels and 40,000 pixels on either edge.</p></>}
    </fieldset>
    <fieldset disabled={disabled}><legend>Color profile</legend>
      <label>Output color space<select value={value.profile} onChange={e => change('profile', e.target.value as Settings['profile'])}><option value="srgb">sRGB</option><option value="linear_srgb">Linear sRGB</option><option value="icc">Choose an RGB ICC profile</option></select></label>
      {value.profile === 'icc' && <><button disabled={choosingProfile} onClick={chooseProfile}>{choosingProfile ? 'Reading profile…' : value.icc ? 'Choose another ICC profile' : 'Choose ICC profile'}</button>{value.icc && <p>{value.icc.name} · {value.icc.bytes} bytes · {value.icc.linear ? 'Linear matrix RGB' : 'RGB'}</p>}</>}
      {float && <p>Float TIFF requires linear sRGB or a linear matrix RGB ICC profile.</p>}
    </fieldset>
    <fieldset disabled={disabled}><legend>Transparency</legend>
      <label>Alpha handling<select value={value.alpha} onChange={e => change('alpha', e.target.value as Settings['alpha'])}><option value="preserve">Preserve transparency</option><option value="composite">Composite on a background</option></select></label>
      {value.alpha === 'composite' && <><p className="hint">Background values use linear RGB from 0 to 1.</p>{(['Red', 'Green', 'Blue'] as const).map((channel, index) => <label key={channel}>{channel} background<input type="number" min="0" max="1" step="any" value={value.background[index]} onChange={e => { const background: Settings['background'] = [...value.background]; background[index] = e.target.value; change('background', background); }} /></label>)}</>}
      {value.format === 'jpeg' && <p>JPEG requires an explicit background color.</p>}
    </fieldset>
    {error && <ErrorNotice message={error} />}
  </section>;
}

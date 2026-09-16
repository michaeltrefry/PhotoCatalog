import { describe, expect, it } from 'vitest';
import { exportOutput, initialExportSettings } from './exportSettings';

describe('export settings contract', () => {
  it('requires explicit JPEG background and a valid quality', () => {
    const settings = initialExportSettings();
    expect(exportOutput(settings).format).toEqual({ format: 'jpeg', quality: 90 });
    expect(() => exportOutput({ ...settings, alpha: 'preserve' })).toThrow('explicit background');
    for (const quality of ['', '0', '101', '90.5', 'NaN']) expect(() => exportOutput({ ...settings, quality })).toThrow('JPEG quality');
    for (const quality of ['1', '100']) expect(exportOutput({ ...settings, quality }).format).toEqual({ format: 'jpeg', quality: Number(quality) });
  });
  it('preserves all integer depths and transparency independently of hidden background fields', () => {
    for (const format of ['png', 'tiff'] as const) for (const depth of ['eight', 'sixteen'] as const) {
      const output = exportOutput({ ...initialExportSettings(), format, depth, alpha: 'preserve', background: ['', '', ''] });
      expect(output.format).toEqual({ format, depth });
      expect(output.alpha).toEqual({ mode: 'preserve' });
    }
    expect(() => exportOutput({ ...initialExportSettings(), format: 'png', depth: 'float32' })).toThrow('PNG supports');
  });
  it('requires a linear profile for float output and sends only the admitted ICC token', () => {
    const settings = { ...initialExportSettings(), format: 'tiff' as const, depth: 'float32' as const };
    expect(() => exportOutput(settings)).toThrow('requires linear');
    expect(exportOutput({ ...settings, profile: 'linear_srgb' }).profile).toEqual({ kind: 'linear_srgb' });
    expect(() => exportOutput({ ...settings, profile: 'icc' })).toThrow('Choose an RGB');
    const icc = { token: 'immutable-profile', name: 'Wide RGB', bytes: '16777216', blake3: 'digest', linear: false };
    expect(() => exportOutput({ ...settings, profile: 'icc', icc })).toThrow('requires linear');
    expect(exportOutput({ ...settings, profile: 'icc', icc: { ...icc, linear: true } }).profile).toEqual({ kind: 'icc', token: 'immutable-profile' });
  });
  it('keeps fit and enlargement explicit without rejecting a box whose actual aspect-fit pixels are unknown', () => {
    const settings = { ...initialExportSettings(), size: 'fit' as const, width: '40000', height: '40000' };
    expect(exportOutput(settings).size).toEqual({ mode: 'fit', width: 40000, height: 40000, allow_upscale: false });
    expect(exportOutput({ ...settings, upscale: true }).size).toEqual({ mode: 'fit', width: 40000, height: 40000, allow_upscale: true });
    for (const width of ['', '0', '40001', '12.5']) expect(() => exportOutput({ ...settings, width })).toThrow('Maximum width');
    expect(exportOutput({ ...settings, size: 'original', width: '' }).size).toEqual({ mode: 'original' });
  });
  it('retains exact composite channels and rejects non-finite or out-of-range values', () => {
    const settings = initialExportSettings();
    expect(exportOutput({ ...settings, background: ['0', '0.25', '1'] }).alpha).toEqual({ mode: 'composite', linear_rgb: [0, 0.25, 1] });
    for (const value of ['', ' ', 'NaN', 'Infinity', '-0.01', '1.01']) expect(() => exportOutput({ ...settings, background: [value, '0', '0'] })).toThrow('Background channels');
  });
});

import type { Decimal } from './bridge';

export type ExportProfile = { token: string; name: string; bytes: Decimal; blake3: string; linear: boolean };
export type ExportOutput = {
  size: { mode: 'original' } | { mode: 'fit'; width: number; height: number; allow_upscale: boolean };
  format: { format: 'jpeg'; quality: number } | { format: 'png'; depth: 'eight' | 'sixteen' } | { format: 'tiff'; depth: 'eight' | 'sixteen' | 'float32' };
  profile: { kind: 'srgb' | 'linear_srgb' } | { kind: 'icc'; token: string };
  alpha: { mode: 'preserve' } | { mode: 'composite'; linear_rgb: [number, number, number] };
};
export type ExportSettings = {
  format: 'jpeg' | 'png' | 'tiff';
  quality: string;
  depth: 'eight' | 'sixteen' | 'float32';
  size: 'original' | 'fit';
  width: string;
  height: string;
  upscale: boolean;
  profile: 'srgb' | 'linear_srgb' | 'icc';
  icc: ExportProfile | null;
  alpha: 'preserve' | 'composite';
  background: [string, string, string];
};
export function initialExportSettings(): ExportSettings {
  return { format: 'jpeg', quality: '90', depth: 'eight', size: 'original', width: '2048', height: '2048', upscale: false, profile: 'srgb', icc: null, alpha: 'composite', background: ['1', '1', '1'] };
}
function integer(text: string, label: string, max: number): number {
  if (!/^\d+$/.test(text) || !Number.isSafeInteger(Number(text)) || Number(text) < 1 || Number(text) > max) throw new Error(`${label} must be a whole number from 1 to ${max}.`);
  return Number(text);
}
/** The renderer remains authoritative for actual dimensions and supplied ICC validity. */
export function exportOutput(settings: ExportSettings): ExportOutput {
  const { format, depth, alpha, profile, icc } = settings;
  if (format === 'jpeg' && alpha !== 'composite') throw new Error('Choose an explicit background for JPEG.');
  if (format === 'png' && depth === 'float32') throw new Error('PNG supports 8-bit or 16-bit output.');
  if (profile === 'icc' && !icc) throw new Error('Choose an RGB ICC profile.');
  if (format === 'tiff' && depth === 'float32' && (profile === 'srgb' || (profile === 'icc' && !icc?.linear))) throw new Error('32-bit float TIFF requires linear sRGB or a linear matrix RGB ICC profile.');
  const rgb = alpha === 'preserve' ? [0, 0, 0] as [number, number, number] : settings.background.map(text => {
    const value = Number(text);
    if (!text.trim() || !Number.isFinite(value) || value < 0 || value > 1) throw new Error('Background channels must be linear RGB values from 0 to 1.');
    return value;
  }) as [number, number, number];
  return {
    size: settings.size === 'original' ? { mode: 'original' } : { mode: 'fit', width: integer(settings.width, 'Maximum width', 40000), height: integer(settings.height, 'Maximum height', 40000), allow_upscale: settings.upscale },
    format: format === 'jpeg' ? { format, quality: integer(settings.quality, 'JPEG quality', 100) } : format === 'png' ? { format, depth: depth as 'eight' | 'sixteen' } : { format, depth },
    profile: profile === 'icc' ? { kind: 'icc', token: icc!.token } : { kind: profile },
    alpha: alpha === 'preserve' ? { mode: 'preserve' } : { mode: 'composite', linear_rgb: rgb },
  };
}

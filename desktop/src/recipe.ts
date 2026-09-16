// Versioned public Rust recipe. Controls display percent but store normalized values.
export interface Recipe {
  version: '1';
  settings: {
    crop: { left: number; top: number; right: number; bottom: number } | null;
    straighten_degrees: number;
    exposure_ev: number;
    white_balance: { mode: 'as_shot' } | { mode: 'temperature_tint'; kelvin: number; tint: number };
    contrast: number;
    highlights: number;
    shadows: number;
    saturation: number;
    vibrance: number;
    sharpening: { amount: number; radius_px: number };
    noise_reduction: { luminance: number; chroma: number };
  };
}

export type AdjustmentGroup = 'geometry' | 'exposure' | 'white_balance' | 'tone' | 'color' | 'sharpening' | 'noise_reduction';

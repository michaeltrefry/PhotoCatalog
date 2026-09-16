import { useEffect, useRef, useState, type ReactNode } from 'react';

export function Section({ title, children, open = true }: { title: string; children: ReactNode; open?: boolean }) {
  return <details className="section" open={open}><summary>{title}</summary><div className="section-body">{children}</div></details>;
}

export function Slider({ label, value, min, max, step, unit = '', onChange, disabled = false }: {
  label: string; value: number; min: number; max: number; step: number; unit?: string;
  onChange: (value: number) => void; disabled?: boolean;
}) {
  const [draft, setDraft] = useState(String(value));
  useEffect(() => setDraft(String(value)), [value]);
  const commit = () => {
    const n = Number(draft);
    if (draft.trim() !== '' && Number.isFinite(n) && n >= min && n <= max) onChange(n);
    else setDraft(String(value));
  };
  return <div className="slider-field">
    <label>{label}<input aria-label={label} type="range" min={min} max={max} step={step} value={value}
      disabled={disabled} onChange={e => onChange(Number(e.target.value))} /></label>
    <input className="numeric" aria-label={`${label} value${unit ? ` in ${unit}` : ''}`} type="number"
      min={min} max={max} step={step} value={draft} disabled={disabled} onChange={e => setDraft(e.target.value)}
      onBlur={commit} onKeyDown={e => { if (e.key === 'Enter') e.currentTarget.blur(); }} />
  </div>;
}

export function Dialog({ title, children, onClose }: { title: string; children: ReactNode; onClose: () => void }) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => { const node = ref.current; node?.showModal(); return () => node?.close(); }, []);
  return <dialog ref={ref} aria-label={title} onCancel={e => { e.preventDefault(); onClose(); }}>
    <header className="dialog-title"><h2>{title}</h2><button aria-label="Close dialog" onClick={onClose}>×</button></header>
    <div className="dialog-body">{children}</div>
  </dialog>;
}

export function ErrorNotice({ message, dismiss }: { message: string; dismiss?: () => void }) {
  return <div className="error-notice" role="alert"><span>{message}</span>{dismiss && <button onClick={dismiss}>Dismiss</button>}</div>;
}

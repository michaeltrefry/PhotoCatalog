import { useEffect, useRef, useState } from 'react';
import { command, errorText, imageKey, previewBlob, type VariantKey } from '../bridge';

// Virtualized cells unmount and remount; their consumer generation must not reset.
let nextGeneration = 0n;

export function usePreview(catalog: string, key: VariantKey, viewport: string, large: boolean, revision: string, interactive = false) {
  const [value, setValue] = useState<{ url?: string; message?: string; loading: boolean }>({ loading: true });
  const generation = useRef(0n);
  const identity = imageKey(key);
  useEffect(() => {
    const current = ++nextGeneration;
    generation.current = current;
    const abort = new AbortController();
    let ticket: string | undefined;
    let url: string | undefined;
    let timer: ReturnType<typeof setTimeout> | undefined;
    setValue({ loading: true });
    const release = () => { void command({ command: 'release_viewport', args: { catalog, viewport, generation: String(current) } }, 'status').catch(() => {}); };
    const cancel = (id: string) => { void command({ command: 'cancel_preview', args: { catalog, ticket: id } }, 'preview').catch(() => {}); };
    const poll = async () => {
      if (abort.signal.aborted || !ticket) return;
      try {
        const state = await command({ command: 'preview_status', args: { catalog, ticket } }, 'preview', abort.signal);
        if (state.generation !== String(current) || imageKey(state.key) !== identity) throw new Error('The preview belongs to an older selection.');
        if (state.state === 'ready') {
          const blob = await previewBlob(catalog, ticket);
          if (abort.signal.aborted || generation.current !== current) return;
          url = URL.createObjectURL(blob);
          setValue({ url, loading: false });
        } else if (state.state === 'queued' || state.state === 'cancel_requested') {
          timer = setTimeout(() => { void poll(); }, 100);
        } else setValue({ loading: false, message: state.message ?? state.state.replaceAll('_', ' ') });
      } catch (e) { if (!abort.signal.aborted) setValue({ loading: false, message: errorText(e) }); }
    };
    void command({ command: 'preview', args: { catalog, key, viewport, generation: String(current), tier: large ? 'large' : 'thumbnail', interactive, foreground: large } }, 'preview', abort.signal)
      .then(state => { ticket = state.ticket; if (abort.signal.aborted) cancel(ticket); else void poll(); })
      .catch(e => { if (!abort.signal.aborted) setValue({ loading: false, message: errorText(e) }); })
      // Native dispatch may admit Preview after the first cleanup release.
      .finally(() => { if (abort.signal.aborted) release(); });
    return () => { abort.abort(); clearTimeout(timer); release(); if (ticket) cancel(ticket); if (url) URL.revokeObjectURL(url); };
    // key is represented by its stable identity; each render's object is not a new consumer.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [catalog, identity, viewport, large, revision, interactive]);
  return value;
}

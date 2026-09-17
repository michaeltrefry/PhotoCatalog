import { useEffect, useRef, useState } from 'react';
import { CatalogError, command, errorText, imageKey, isBusyError, logPreviewDiagnostic, previewBlob, type PreviewStatus, type VariantKey } from '../bridge';
import { measurementDiagnosticsEnabled } from '../performanceMeasurement';
import { PreviewCleanupCapacityError, PreviewCleanupCoordinator, type PreviewCleanupLease } from '../previewCleanup';

// Virtualized cells unmount and remount; their consumer generation must not reset.
let nextGeneration = 0n;
const cleanup = new PreviewCleanupCoordinator(
  async (catalog, viewport, generation) => {
    await command({ command: 'release_viewport', args: { catalog, viewport, generation } }, 'status');
  },
  error => isBusyError(error) ? 'busy' : error instanceof CatalogError && ['closed', 'stale_session'].includes(error.code) ? 'retired' : 'failure',
);

export function usePreview(catalog: string, key: VariantKey, viewport: string, large: boolean, revision: string, interactive = false, attempt = 0) {
  const [value, setValue] = useState<{ url?: string; message?: string; loading: boolean }>({ loading: true });
  const generation = useRef(0n);
  const identity = imageKey(key);
  useEffect(() => {
    const current = ++nextGeneration;
    generation.current = current;
    const abort = new AbortController();
    let lease: PreviewCleanupLease | undefined;
    let ticket: string | undefined;
    let url: string | undefined;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const diagnostics = measurementDiagnosticsEnabled();
    const diagnosticStarted = diagnostics ? performance.now() : 0;
    let admissionCommandMs = 0;
    let polls = 0;
    setValue({ loading: true });
    const poll = async () => {
      if (abort.signal.aborted || !ticket) return;
      try {
        polls += 1;
        const state = await command({ command: 'preview_status', args: { catalog, ticket } }, 'preview', abort.signal);
        if (state.generation !== String(current) || imageKey(state.key) !== identity) throw new Error('The preview belongs to an older selection.');
        if (state.state === 'ready') {
          const readyObservedMs = diagnostics ? performance.now() - diagnosticStarted : 0;
          const blobStarted = diagnostics ? performance.now() : 0;
          const blob = await previewBlob(catalog, ticket);
          if (abort.signal.aborted || generation.current !== current) return;
          const blobInvokeMs = diagnostics ? performance.now() - blobStarted : 0;
          const objectUrlStarted = diagnostics ? performance.now() : 0;
          url = URL.createObjectURL(blob);
          const objectUrlMs = diagnostics ? performance.now() - objectUrlStarted : 0;
          setValue({ url, loading: false });
          if (diagnostics) {
            const deliveredTicket = ticket;
            void command({ command: 'preview_status', args: { catalog, ticket } }, 'preview', abort.signal)
              .then((finalState: PreviewStatus) => {
                if (abort.signal.aborted || generation.current !== current || !finalState.diagnostic) return;
                return logPreviewDiagnostic({
                  ticket: deliveredTicket,
                  admission_command_ms: admissionCommandMs,
                  ready_observed_ms: readyObservedMs,
                  blob_invoke_ms: blobInvokeMs,
                  object_url_ms: objectUrlMs,
                  polls,
                  native: finalState.diagnostic,
                });
              })
              .catch(() => { /* Diagnostic readback must not change preview delivery. */ });
          }
        } else if (state.state === 'queued' || state.state === 'cancel_requested') {
          const message = state.message ?? 'Preparing preview…';
          setValue(previous => previous.loading && previous.message === message ? previous : { loading: true, message });
          timer = setTimeout(() => { void poll(); }, 100);
        } else setValue({ loading: false, message: state.message ?? state.state.replaceAll('_', ' ') });
      } catch (e) { if (!abort.signal.aborted) setValue({ loading: false, message: errorText(e) }); }
    };
    const admit = () => {
      if (abort.signal.aborted) return;
      const reserved = cleanup.reserve(catalog, viewport, String(current));
      lease = reserved;
      void reserved.ready.then(async ready => {
        if (!ready || !reserved.begin()) return;
        let admitted = false;
        try {
          const state = await command({ command: 'preview', args: { catalog, key, viewport, generation: String(current), tier: large ? 'large' : 'thumbnail', interactive, foreground: large, diagnostics } }, 'preview', abort.signal);
          admitted = true; admissionCommandMs = diagnostics ? performance.now() - diagnosticStarted : 0; ticket = state.ticket;
          if (!abort.signal.aborted) void poll();
        } catch (e) {
          if (!abort.signal.aborted) setValue({ loading: false, message: errorText(e) });
        } finally {
          reserved.settled();
          // ReleaseViewport is authoritative: it removes pending work and
          // ticket ownership; the actor then cancels the obsolete consumer.
          if (!admitted || abort.signal.aborted) reserved.release();
        }
      }).catch(e => {
        if (abort.signal.aborted) return;
        if (e instanceof PreviewCleanupCapacityError) {
          setValue({ loading: true, message: e.message });
          timer = setTimeout(admit, 100);
        } else setValue({ loading: false, message: errorText(e) });
      });
    };
    admit();
    return () => { abort.abort(); clearTimeout(timer); lease?.release(); if (url) URL.revokeObjectURL(url); };
    // key is represented by its stable identity; each render's object is not a new consumer.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [catalog, identity, viewport, large, revision, interactive, attempt]);
  return value;
}

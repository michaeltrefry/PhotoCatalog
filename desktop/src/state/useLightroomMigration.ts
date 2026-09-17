import { useCallback, useEffect, useRef, useState } from 'react';
import { CatalogError, errorText } from '../bridge';
import { migration, sameGuard, type Guard, type Request, type Response, type Snapshot } from '../lightroomMigration';

const STORAGE = 'lensworks.lightroom-migration.guard.v1';
const validGuard = (value: unknown): value is Guard => {
  if (!value || typeof value !== 'object') return false;
  const item = value as Partial<Guard>;
  return [item.session, item.generation, item.operation].every(id => typeof id === 'string' && /^[A-Za-z0-9_-]{1,64}$/.test(id));
};
function restoredGuard(): Guard | null {
  try { const value: unknown = JSON.parse(sessionStorage.getItem(STORAGE) ?? 'null'); return validGuard(value) ? value : null; }
  catch { return null; }
}
function retain(guard: Guard | null) {
  try { if (guard) sessionStorage.setItem(STORAGE, JSON.stringify(guard)); else sessionStorage.removeItem(STORAGE); }
  catch { /* Status remains guarded in memory when session storage is unavailable. */ }
}

/** One app-owned migration controller. Dialog visibility never owns the native
 * operation; polling and the exact guard survive panel close and page reload. */
export function useLightroomMigration(enabled: boolean) {
  const initial = useRef<Guard | null>(null);
  if (initial.current === null && enabled) initial.current = restoredGuard();
  const guard = useRef<Guard | null>(initial.current);
  const current = useRef<Snapshot | null>(null);
  const [snapshot, setSnapshot] = useState<Snapshot | null>(null);
  const [ready, setReady] = useState(!initial.current), [busy, setBusy] = useState(false);
  const [error, setError] = useState(''), [statusError, setStatusError] = useState('');
  const [outcomeUnknown, setOutcomeUnknown] = useState(false), [stale, setStale] = useState(false);
  const busyRef = useRef(false), mounted = useRef(true);
  const accept = useCallback((reply: Response) => {
    if (reply.kind === 'status') {
      guard.current = reply.data.guard; current.current = reply.data; retain(reply.data.guard);
      setSnapshot(reply.data); setStale(false); setOutcomeUnknown(false); setStatusError(''); setReady(true);
    } else if (reply.kind === 'discarded') {
      if (guard.current && sameGuard(reply.data.guard, guard.current)) { guard.current = null; current.current = null; retain(null); setSnapshot(null); setStale(false); setOutcomeUnknown(false); }
    }
    return reply;
  }, []);
  const request = useCallback(async (value: Request): Promise<Response> => {
    if (busyRef.current) throw new Error('A migration command is already pending; wait for status reconciliation before replaying it.');
    busyRef.current = true; if (mounted.current) setBusy(true);
    try { const reply = accept(await migration(value)); if (mounted.current) setError(''); return reply; }
    catch (cause) {
      if (mounted.current) {
        setError(errorText(cause));
        if (!(cause instanceof CatalogError)) setOutcomeUnknown(true);
        if (cause instanceof CatalogError && cause.code === 'stale_session') setStale(true);
      }
      throw cause;
    } finally { busyRef.current = false; if (mounted.current) setBusy(false); }
  }, [accept]);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  useEffect(() => {
    if (!enabled) return;
    const abort = new AbortController(); let timer: ReturnType<typeof setTimeout>;
    const poll = async () => {
      const known = guard.current;
      if (known && !stale && !busyRef.current) {
        try {
          const reply = await migration({ action: 'status', guard: known });
          if (!abort.signal.aborted && reply.kind === 'status' && sameGuard(reply.data.guard, known)) accept(reply);
        } catch (cause) {
          if (!abort.signal.aborted) {
            setReady(true); setStatusError(errorText(cause));
            if (cause instanceof CatalogError && cause.code === 'stale_session') setStale(true);
          }
        }
      } else if (!known) setReady(true);
      if (!abort.signal.aborted) timer = setTimeout(() => { void poll(); }, 500);
    };
    void poll(); return () => { abort.abort(); clearTimeout(timer); };
  }, [enabled, stale, accept]);
  const forgetStale = useCallback(() => {
    if (!stale) return;
    guard.current = null; current.current = null; retain(null); setSnapshot(null); setStale(false); setOutcomeUnknown(false); setError(''); setStatusError(''); setReady(true);
  }, [stale]);
  return { snapshot, current, ready, busy, error, statusError, outcomeUnknown, stale, request, forgetStale };
}

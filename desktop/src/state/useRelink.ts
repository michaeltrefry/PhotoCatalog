import { useEffect, useRef, useState } from 'react';
import { errorText } from '../bridge';
import { relink, relinkTerminal, type RelinkOperation, type RelinkRequest } from '../relink';

/** Discover and observe an owned operation; reopening never starts saved work. */
export function useRelink(catalog: string | null) {
  const [operation, setOperation] = useState<RelinkOperation | null>(null); const [ready, setReady] = useState(false); const [admitting, setAdmitting] = useState(false); const [error, setError] = useState('');
  const [discovery, setDiscovery] = useState(0);
  const session = useRef(catalog); session.current = catalog;
  const admission = useRef(false); const alive = useRef(true);
  useEffect(() => { const abort = new AbortController(); alive.current = true; setReady(false); setOperation(null); setError('');
    if (!catalog) { setReady(true); return () => { alive.current = false; abort.abort(); }; }
    void relink(catalog, { command: 'status', args: { operation: null } }, 'operation', abort.signal).then(value => { if (!abort.signal.aborted) { setOperation(value); setReady(true); } }).catch(e => { if (!abort.signal.aborted) setError(errorText(e)); });
    return () => { alive.current = false; abort.abort(); };
  }, [catalog, discovery]);
  const currentOperation = useRef(operation); currentOperation.current = operation;
  const id = operation?.id; const terminal = operation ? relinkTerminal(operation) : true;
  useEffect(() => {
    if (!catalog || !id || terminal) return;
    const abort = new AbortController(); let timer: ReturnType<typeof setTimeout> | undefined;
    const poll = async () => {
      try { const current = await relink(catalog, { command: 'status', args: { operation: id } }, 'operation', abort.signal); if (abort.signal.aborted) return; setOperation(previous => previous?.id === id ? current : previous); if (currentOperation.current?.id === id) setError(''); if (current && !relinkTerminal(current)) timer = setTimeout(() => void poll(), 500); }
      catch (e) { if (!abort.signal.aborted && currentOperation.current?.id === id) { setError(errorText(e)); timer = setTimeout(() => void poll(), 1500); } }
    };
    timer = setTimeout(() => void poll(), 250);
    return () => { abort.abort(); clearTimeout(timer); };
  }, [catalog, id, terminal]);
  const start = async (request: RelinkRequest) => {
    if (!catalog || !ready || admission.current || operation && !relinkTerminal(operation)) throw new Error('Wait for the current storage operation to finish.');
    admission.current = true; setAdmitting(true); setError('');
    try { const next = await relink(catalog, request, 'operation'); if (alive.current && session.current === catalog) { if (!next) throw new Error('Storage operation was not admitted.'); setOperation(next); } }
    finally { admission.current = false; if (alive.current && session.current === catalog) setAdmitting(false); }
  };
  const cancel = async () => { if (!catalog || !operation) return; try { const next = await relink(catalog, { command: 'cancel', args: { operation: operation.id } }, 'operation'); if (alive.current && session.current === catalog) setOperation(previous => previous?.id === operation.id ? next : previous); } catch (e) { if (alive.current && session.current === catalog && currentOperation.current?.id === operation.id) setError(errorText(e)); } };
  return { operation, ready, retry: () => setDiscovery(v => v + 1), busy: admitting || !!operation && !relinkTerminal(operation), error, start, cancel };
}

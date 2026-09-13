import { useEffect, useRef, useState } from 'react';
import { errorText } from '../bridge';
import { copyTerminal, editCopy, type CopyOperation } from '../editCopy';

/** Observe saved work without resuming it; the catalog owns execution. */
export function useEditCopy(catalog: string | null) {
  const [operation, setOperation] = useState<CopyOperation | null>(null);
  const [ready, setReady] = useState(false), [admitting, setAdmitting] = useState(false);
  const [error, setError] = useState(''), [retry, setRetry] = useState(0);
  const [unknown, setUnknown] = useState(false);
  const live = useRef({ catalog, epoch: 0, alive: false });
  const current = useRef(operation); current.current = operation;
  const admission = useRef<symbol | null>(null);
  useEffect(() => {
    const context = { catalog, epoch: live.current.epoch + 1, alive: true }; live.current = context;
    const abort = new AbortController(); setReady(false); setOperation(null); current.current = null; setError(''); setAdmitting(false); admission.current = null; setUnknown(true);
    if (!catalog) { setReady(true); setUnknown(false); }
    else void editCopy(catalog, { command: 'status', args: { operation: null } }, 'operation', abort.signal)
      .then(value => { if (!abort.signal.aborted) { current.current = value; setOperation(value); setReady(true); setUnknown(false); } })
      .catch(e => { if (!abort.signal.aborted) setError(errorText(e)); });
    return () => { context.alive = false; abort.abort(); };
  }, [catalog, retry]);
  const id = operation?.id, terminal = !operation || copyTerminal(operation);
  useEffect(() => {
    if (!catalog || !id || terminal) return;
    const abort = new AbortController(); let timer: ReturnType<typeof setTimeout>;
    const poll = async () => {
      try {
        const value = await editCopy(catalog, { command: 'status', args: { operation: id } }, 'operation', abort.signal);
        if (abort.signal.aborted || current.current?.id !== id) return;
        if (!value) throw new Error('Adjustment copy status is unavailable.');
        current.current = value; setOperation(value); setError('');
        if (!copyTerminal(value)) timer = setTimeout(() => void poll(), 500);
      } catch (e) { if (!abort.signal.aborted && current.current?.id === id) { setError(errorText(e)); timer = setTimeout(() => void poll(), 1500); } }
    };
    timer = setTimeout(() => void poll(), 250);
    return () => { abort.abort(); clearTimeout(timer); };
  }, [catalog, id, terminal]);
  const run = async (job: string) => {
    if (!catalog || !ready || admission.current || current.current && !copyTerminal(current.current)) throw new Error('Wait for the current adjustment copy to finish.');
    const context = live.current, token = Symbol(); admission.current = token; setAdmitting(true); setError('');
    try {
      const value = await editCopy(catalog, { command: 'run', args: { job } }, 'operation');
      if (!value) throw new Error('Adjustment copy was not started.');
      if (context.alive && live.current === context) { current.current = value; setOperation(value); }
    } catch (runError) {
      if (context.alive && live.current === context) {
        setUnknown(true); setReady(false);
        try {
          const recovered = await editCopy(catalog, { command: 'status', args: { operation: null } }, 'operation');
          if (context.alive && live.current === context) { current.current = recovered; setOperation(recovered); setReady(true); setUnknown(false); }
        } catch (statusError) {
          if (context.alive && live.current === context) setError(`The start reply was lost and copy status could not be recovered: ${errorText(statusError)}`);
        }
      }
      throw runError;
    } finally { if (admission.current === token) { admission.current = null; if (context.alive) setAdmitting(false); } }
  };
  const cancel = async () => {
    const target = current.current, context = live.current;
    if (!catalog || !target || copyTerminal(target)) return;
    try {
      const value = await editCopy(catalog, { command: 'cancel', args: { job: target.job.id, operation: target.id } }, 'operation');
      if (context.alive && live.current === context && current.current?.id === target.id && value) { current.current = value; setOperation(value); }
    } catch (e) { if (context.alive && live.current === context && current.current?.id === target.id) setError(errorText(e)); }
  };
  return { operation, ready, busy: unknown || admitting || !!operation && !copyTerminal(operation), error, run, cancel, retry: () => setRetry(v => v + 1) };
}

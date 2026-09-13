import { useEffect, useRef, useState } from 'react';
import { errorText } from '../bridge';
import { copyTerminal, editCopy, type CopyOperation } from '../editCopy';

type Scope = { catalog: string | null; alive: boolean };
type Admission = { scope: Scope; previous: string | null; job: string; failed: boolean; resolve: () => void; reject: (error: unknown) => void };

/** Observe saved work independently of Run replies; catalog owns execution. */
export function useEditCopy(catalog: string | null) {
  const [operation, setOperation] = useState<CopyOperation | null>(null);
  const [ready, setReady] = useState(false), [admitting, setAdmitting] = useState(false);
  const [error, setError] = useState(''), [retry, setRetry] = useState(0);
  const live = useRef<Scope>({ catalog, alive: false });
  const current = useRef<CopyOperation | null>(null);
  const admission = useRef<Admission | null>(null);
  const canceling = useRef<string | null>(null);
  const readEpoch = useRef(0);
  useEffect(() => {
    const context = { catalog, alive: true }; live.current = context;
    const abort = new AbortController(); let timer: ReturnType<typeof setTimeout>;
    setReady(false); setOperation(null); current.current = null; setError(''); setAdmitting(false); admission.current = null; canceling.current = null;
    const poll = async () => {
      const epoch = readEpoch.current;
      try {
        const value = await editCopy(catalog!, { command: 'status', args: { operation: null } }, 'operation', abort.signal);
        if (abort.signal.aborted || epoch !== readEpoch.current) return;
        // Only the serial polling loop publishes status. Late Run/Cancel replies
        // cannot regress completed work or replace a newer operation.
        if (JSON.stringify(current.current) !== JSON.stringify(value)) { current.current = value; setOperation(value); }
        setReady(true); setError('');
        const ticket = admission.current;
        if (value && ticket?.scope === context && value.id !== ticket.previous) {
          admission.current = null; setAdmitting(false);
          if (value.job.id === ticket.job) ticket.resolve();
          else ticket.reject(new Error('Another copy operation became active. Review saved batches before retrying.'));
        } else if (ticket?.scope === context && ticket.failed) {
          // Only a fresh read started after the failed acknowledgement can
          // release uncertainty; an older in-flight reply is fenced above.
          admission.current = null; setAdmitting(false);
        }
        if (canceling.current && (value?.id !== canceling.current || copyTerminal(value) || value.phase === 'cancel_requested')) canceling.current = null;
      } catch (e) { if (!abort.signal.aborted && epoch === readEpoch.current) { setReady(false); setError(errorText(e)); } }
      finally { if (!abort.signal.aborted) timer = setTimeout(() => void poll(), 500); }
    };
    if (catalog) void poll(); else setReady(true);
    return () => {
      context.alive = false; abort.abort(); clearTimeout(timer);
      if (admission.current?.scope === context) {
        admission.current.reject(new Error('Catalog session changed; inspect saved batches after reopening.'));
        admission.current = null;
      }
    };
  }, [catalog, retry]);
  const run = async (job: string) => {
    if (!catalog || !ready || admission.current || current.current && !copyTerminal(current.current)) throw new Error('Wait for the current adjustment copy to finish.');
    const context = live.current;
    readEpoch.current += 1; // Ignore a status request sent before this admission.
    const observed = new Promise<void>((resolve, reject) => {
      const ticket = { scope: context, previous: current.current?.id ?? null, job, failed: false, resolve, reject };
      admission.current = ticket; setAdmitting(true); setError('');
      void editCopy(catalog, { command: 'run', args: { job } }, 'operation').then(value => {
        if (!value) throw new Error('Adjustment copy was not started.');
        // Resolve through independent status observation, even if this reply
        // never arrives. The caller can then release its own action gate.
      }).catch(e => {
        if (context.alive && live.current === context && admission.current === ticket) {
          readEpoch.current += 1; ticket.failed = true; setReady(false); setError(errorText(e)); ticket.reject(e);
        }
      });
    });
    await observed;
  };
  const cancel = async () => {
    const target = current.current, context = live.current;
    if (!catalog || !target || copyTerminal(target) || canceling.current === target.id) return;
    canceling.current = target.id;
    try { await editCopy(catalog, { command: 'cancel', args: { job: target.job.id, operation: target.id } }, 'operation'); }
    catch (e) {
      if (context.alive && live.current === context && canceling.current === target.id) {
        canceling.current = null; setError(errorText(e));
      }
    }
  };
  return { operation, ready, busy: !ready || admitting || !!operation && !copyTerminal(operation), error, run, cancel, retry: () => setRetry(v => v + 1) };
}

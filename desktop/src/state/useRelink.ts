import { useEffect, useRef, useState } from 'react';
import { errorText } from '../bridge';
import { relink, relinkTerminal, type RelinkAction, type RelinkOperation, type RelinkRequest } from '../relink';

type Scope = { catalog: string | null; alive: boolean };
type Admission = { scope: Scope; previous: string | null; action: RelinkAction; plan: string | null; failed: boolean; resolve: () => void; reject: (reason: unknown) => void };

/** Catalog-owned status observation never replays a storage action. */
export function useRelink(catalog: string | null) {
  const [operation, setOperation] = useState<RelinkOperation | null>(null);
  const [ready, setReady] = useState(false), [admitting, setAdmitting] = useState(false), [error, setError] = useState('');
  const scope = useRef<Scope>({ catalog, alive: false }), current = useRef<RelinkOperation | null>(null);
  const admission = useRef<Admission | null>(null), canceling = useRef<string | null>(null);
  const readEpoch = useRef(0), statusKnown = useRef(false);
  useEffect(() => {
    const context = { catalog, alive: true }; scope.current = context;
    const abort = new AbortController(); let timer: ReturnType<typeof setTimeout>;
    current.current = null; statusKnown.current = false; canceling.current = null;
    setOperation(null); setReady(false); setAdmitting(false); setError('');
    const poll = async () => {
      const epoch = readEpoch.current;
      try {
        const value = await relink(catalog!, { command: 'status', args: { operation: null } }, 'operation', abort.signal);
        if (abort.signal.aborted || epoch !== readEpoch.current) return;
        if (JSON.stringify(value) !== JSON.stringify(current.current)) { current.current = value; setOperation(value); }
        statusKnown.current = true; setReady(true); setError('');
        const ticket = admission.current;
        if (value && ticket?.scope === context && value.id !== ticket.previous) {
          admission.current = null; setAdmitting(false);
          if (value.action === ticket.action && (ticket.plan === null || value.plan?.id === ticket.plan)) ticket.resolve();
          else ticket.reject(new Error('Another storage operation became active. Inspect its result before retrying.'));
        } else if (ticket?.scope === context && ticket.failed) {
          // Only a read begun after the failed acknowledgement resolves this
          // uncertainty; earlier in-flight status is fenced by readEpoch.
          admission.current = null; setAdmitting(false);
        }
        if (canceling.current && (value?.id !== canceling.current || relinkTerminal(value) || value.phase === 'cancel_requested')) canceling.current = null;
      } catch (e) {
        if (!abort.signal.aborted && epoch === readEpoch.current) { statusKnown.current = false; setReady(false); setError(errorText(e)); }
      } finally { if (!abort.signal.aborted) timer = setTimeout(() => void poll(), 500); }
    };
    if (catalog) void poll(); else setReady(true);
    return () => {
      context.alive = false; abort.abort(); clearTimeout(timer);
      if (admission.current?.scope === context) {
        admission.current.reject(new Error('Catalog session changed; inspect saved relink work after reopening.'));
        admission.current = null;
      }
    };
  }, [catalog]);
  const start = async (request: RelinkRequest) => {
    const context = scope.current;
    if (!context.alive || context.catalog !== catalog) throw new Error('Catalog session changed; reopen Locate originals.');
    if (!catalog || !statusKnown.current || admission.current || current.current && !relinkTerminal(current.current)) throw new Error('Wait for the current storage operation or recover its status.');
    if (!['prepare', 'confirm', 'revise', 'apply', 'undo', 'mounts', 'original'].includes(request.command)) throw new Error('This request does not start a storage operation.');
    const action = request.command as RelinkAction;
    const plan = 'args' in request && 'plan' in request.args ? request.args.plan : null;
    readEpoch.current += 1;
    return new Promise<void>((resolve, reject) => {
      const ticket: Admission = { scope: context, previous: current.current?.id ?? null, action, plan, failed: false, resolve, reject };
      admission.current = ticket; setAdmitting(true); setError('');
      void relink(catalog, request, 'operation').then(value => {
        if (!value) throw new Error('Storage operation was not admitted.');
      }).catch(e => {
        if (context.alive && scope.current === context && admission.current === ticket) {
          readEpoch.current += 1; ticket.failed = true; statusKnown.current = false; setReady(false); setError(errorText(e)); reject(e);
        }
      });
    });
  };
  const cancel = async () => {
    const context = scope.current, target = current.current;
    if (!context.alive || context.catalog !== catalog) throw new Error('Catalog session changed; reopen Locate originals.');
    if (!catalog || !target || relinkTerminal(target) || canceling.current === target.id) return;
    canceling.current = target.id;
    try { await relink(catalog, { command: 'cancel', args: { operation: target.id } }, 'operation'); }
    catch (e) { if (context.alive && scope.current === context && canceling.current === target.id) { canceling.current = null; setError(errorText(e)); } }
  };
  const retry = () => {
    if (!scope.current.alive || scope.current.catalog !== catalog) return;
    readEpoch.current += 1; statusKnown.current = false; setReady(false); setError('Rechecking storage operation status…');
  };
  return { operation, ready, retry, busy: !ready || admitting || !!operation && !relinkTerminal(operation), writeHeld: !ready || admitting || !!operation?.write_hold, error, start, cancel };
}

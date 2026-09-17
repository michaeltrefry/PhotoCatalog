import { useEffect, useRef, useState } from 'react';
import { CatalogError, errorText } from '../bridge';
import { inspectionTerminal, lightroom, type Action, type Guard, type Options, type Query, type Request, type Status } from '../lightroom';

type Open = Extract<Request, { kind: 'Open' }>;
type Start = Open | Extract<Request, { kind: 'Action' | 'Read' }>;
type Lifetime = { alive: boolean };
export type InspectionAdmission = { operation: string; completion: Promise<Status> };
type Ticket = {
  lifetime: Lifetime; request: Start; previous: Status | null; observed: string | null; backendRejected: boolean;
  resolve: (value: InspectionAdmission) => void; reject: (reason: unknown) => void;
  complete: (value: Status) => void; interrupt: (reason: unknown) => void;
  completion: Promise<Status>;
};
export const inspectionGuard = (s: Status): Guard => ({ workbench: s.workbench, generation: s.generation, operation: s.operation });
export const sameInspection = (s: Status | null, g: Guard) => !!s && s.workbench === g.workbench && s.generation === g.generation && s.operation === g.operation;

/** App-owned inspection survives foreground catalog and dialog changes. Status
 * polling observes accepted work; it never starts, imports or resumes anything. */
export function useLightroom(enabled: boolean) {
  const [status, setStatus] = useState<Status | null>(null);
  const [options, setOptions] = useState<Options | null>(null);
  const [ready, setReady] = useState(false), [admitting, setAdmitting] = useState(false);
  const [optionEpoch, setOptionEpoch] = useState(0);
  const [closePending, setClosePending] = useState<string | null>(null);
  const [closeError, setCloseError] = useState('');
  const [error, setError] = useState('');
  const [statusError, setStatusError] = useState(''), [optionError, setOptionError] = useState('');
  const lifetime = useRef<Lifetime>({ alive: false });
  const current = useRef<Status | null>(null), known = useRef(false);
  const pending = useRef<Ticket | null>(null), epoch = useRef(0);
  const stopping = useRef<string | null>(null);
  const closing = useRef<{ workbench: string; backendRejected: boolean; error: string } | null>(null);
  const renderLifetime = lifetime.current;
  useEffect(() => {
    const context = { alive: true }; lifetime.current = context;
    const abort = new AbortController(); let timer: ReturnType<typeof setTimeout>;
    current.current = null; known.current = false; setStatus(null); setReady(false); setOptions(null);
    setAdmitting(false); setClosePending(null); setCloseError(''); setError(''); setStatusError(''); setOptionError(''); stopping.current = null; closing.current = null;
    const clear = (ticket: Ticket) => { if (pending.current === ticket) { pending.current = null; setAdmitting(false); } };
    const poll = async () => {
      const read = epoch.current;
      try {
        const reply = await lightroom({ kind: 'Status', workbench: null, attempt: null }, abort.signal);
        if (abort.signal.aborted || read !== epoch.current) return;
        if (reply.kind !== 'Status') throw new Error('Unexpected Lightroom status response.');
        const value = reply.value;
        if (JSON.stringify(current.current) !== JSON.stringify(value)) { current.current = value; setStatus(value); }
        known.current = true; setReady(true); setStatusError('');
        const ticket = pending.current;
        if (ticket?.lifetime === context) {
          const matches = value && (ticket.request.kind === 'Open'
            ? value.attempt === ticket.request.attempt
            : value.workbench === ticket.request.guard.workbench && value.operation !== ticket.request.guard.operation);
          if (!ticket.observed && matches) {
            ticket.observed = value.operation; setAdmitting(false); setError('');
            ticket.resolve({ operation: value.operation, completion: ticket.completion });
          }
          if (ticket.observed) {
            if (!value || value.operation !== ticket.observed) {
              ticket.interrupt(new Error('Inspection operation changed. Review its current status.')); clear(ticket);
            } else if (inspectionTerminal(value)) { ticket.complete(value); clear(ticket); }
          } else if (value?.closed && ticket.previous?.workbench === value.workbench && ticket.request.kind !== 'Open') {
            const reason = new Error('Inspection closed before this operation was observed. Reopen it to review saved work.');
            ticket.reject(reason); ticket.interrupt(reason); clear(ticket);
          } else if (ticket.backendRejected) {
            // This read began after the backend's error reply. An accepted start
            // would already have installed its new cached identity before replying.
            ticket.interrupt(new Error('Inspection admission was rejected.')); clear(ticket);
          }
        }
        if (stopping.current && (!value || value.operation !== stopping.current || inspectionTerminal(value) || value.phase === 'CancelRequested')) stopping.current = null;
        const close = closing.current;
        if (close && (!value || value.workbench !== close.workbench || value.closed || close.backendRejected && inspectionTerminal(value))) {
          closing.current = null; setClosePending(null);
          setCloseError(close.backendRejected && value?.workbench === close.workbench && !value.closed ? `${close.error} The inspection remains open.` : '');
        }
      } catch (e) {
        if (!abort.signal.aborted && read === epoch.current) { known.current = false; setReady(false); setStatusError(errorText(e)); }
      } finally { if (!abort.signal.aborted) timer = setTimeout(() => void poll(), 500); }
    };
    if (enabled) void poll();
    return () => {
      context.alive = false; abort.abort(); clearTimeout(timer);
      const ticket = pending.current;
      if (ticket?.lifetime === context) {
        const reason = new Error('Inspection controls closed. Inspect saved work when the application opens again.');
        ticket.reject(reason); ticket.interrupt(reason); pending.current = null;
      }
    };
  }, [enabled]);
  useEffect(() => {
    const abort = new AbortController();
    if (enabled) void lightroom({ kind: 'Options' }, abort.signal).then(reply => {
      if (abort.signal.aborted) return;
      if (reply.kind !== 'Options') throw new Error('Unexpected Lightroom options response.');
      setOptions(reply.value); setOptionError('');
    }).catch(e => { if (!abort.signal.aborted) setOptionError(errorText(e)); });
    return () => abort.abort();
  }, [enabled, optionEpoch]);
  const guard = (): Guard => {
    const value = current.current;
    if (!enabled || !renderLifetime.alive || lifetime.current !== renderLifetime || !known.current || !value || !status || !sameInspection(value, inspectionGuard(status)) || value.closed || !value.initialized || !inspectionTerminal(value) || pending.current || closing.current) throw new Error('Wait for inspection to finish or recover its status.');
    return inspectionGuard(value);
  };
  const admit = (request: Start): Promise<InspectionAdmission> => {
    const context = renderLifetime, previous = current.current;
    if (!enabled || !context.alive || context !== lifetime.current || !known.current || pending.current || closing.current) return Promise.reject(new Error('Recover inspection status before starting another operation.'));
    if (request.kind === 'Open') {
      if (status ? !sameInspection(previous, inspectionGuard(status)) : previous !== null) return Promise.reject(new Error('Inspection changed while choosing its location. Review the current inspection.'));
      if (previous && !previous.closed) return Promise.reject(new Error('Close the current inspection before opening another.'));
    } else {
      try { if (!sameInspection(previous, guard())) throw new Error('Inspection changed.'); }
      catch (e) { return Promise.reject(e); }
      if (!sameInspection(previous, request.guard)) return Promise.reject(new Error('Inspection input belongs to an earlier operation. Review it again.'));
    }
    epoch.current += 1; setError(''); setAdmitting(true);
    return new Promise((resolve, reject) => {
      let complete!: Ticket['complete'], interrupt!: Ticket['interrupt'];
      const completion = new Promise<Status>((yes, no) => { complete = yes; interrupt = no; });
      void completion.catch(() => {});
      const ticket: Ticket = { lifetime: context, request, previous, observed: null, backendRejected: false, resolve, reject, complete, interrupt, completion };
      pending.current = ticket;
      // Do not abort mutation transport when a dialog hides. The exact reply and
      // independent cached status remain available to reconcile a late start.
      void lightroom(request).catch(e => {
        if (context.alive && lifetime.current === context && pending.current === ticket && !ticket.observed) {
          ticket.backendRejected = e instanceof CatalogError;
          epoch.current += 1; known.current = false; setReady(false); setError(`${errorText(e)} Inspect status before closing or reopening this inspection.`);
          reject(e);
          // A transport error is not proof that the queued operation was rejected.
          // Keep admission until its new identity is observed or the owner closes.
        }
      });
    });
  };
  const cancel = async () => {
    const value = status, context = renderLifetime;
    if (!context.alive || context !== lifetime.current || !value || !sameInspection(current.current, inspectionGuard(value)) || inspectionTerminal(value) || stopping.current === value.operation) return;
    stopping.current = value.operation;
    try { await lightroom({ kind: 'Cancel', guard: inspectionGuard(value) }); }
    catch (e) { if (context.alive && lifetime.current === context && stopping.current === value.operation) { stopping.current = null; setError(errorText(e)); } }
  };
  const close = async (retrying = false) => {
    const value = status, context = renderLifetime;
    if (!context.alive || context !== lifetime.current || !value || current.current?.workbench !== value.workbench || value.closed || !retrying && closing.current?.workbench === value.workbench) return;
    const ticket = { workbench: value.workbench, backendRejected: false, error: '' };
    closing.current = ticket; setClosePending(value.workbench); setCloseError('');
    try { await lightroom({ kind: 'Close', workbench: value.workbench }); }
    catch (e) {
      if (context.alive && lifetime.current === context && closing.current === ticket) {
        ticket.backendRejected = e instanceof CatalogError;
        ticket.error = errorText(e);
        epoch.current += 1; known.current = false; setReady(false);
        setCloseError(`${ticket.error} Closing remains pending until its status is confirmed. You can explicitly retry closing this inspection. If retry still cannot confirm closure, quit and reopen LensWorks before opening another Lightroom inspection.`);
      }
    }
  };
  const retry = () => {
    if (!renderLifetime.alive || lifetime.current !== renderLifetime) return;
    epoch.current += 1; known.current = false; setReady(false); setOptionEpoch(v => v + 1); setStatusError('Rechecking inspection status…');
  };
  return {
    status, options, ready, admitting, closePending, error: [statusError, optionError, closeError, error].filter(Boolean).join(' '),
    busy: !ready || admitting || closePending !== null || !!status && !inspectionTerminal(status),
    guard, current: () => renderLifetime.alive && lifetime.current === renderLifetime ? current.current : null,
    open: (request: Omit<Open, 'kind' | 'attempt'>) => admit({ ...request, kind: 'Open', attempt: crypto.randomUUID() }),
    action: async (action: Action, expected?: Guard) => admit({ kind: 'Action', guard: expected ?? guard(), action }),
    read: async (query: Query, expected?: Guard) => admit({ kind: 'Read', guard: expected ?? guard(), query }),
    cancel, close: () => close(), retryClose: () => close(true), retry,
  };
}

import { useEffect, useRef, useState } from 'react';
import { errorText } from '../bridge';
import { photoExport, terminal, type Operation, type OperationKind, type Request } from '../photoExport';

export type ExportAction = Extract<Request, { command: Exclude<OperationKind, 'cancel'> }>;
type Scope = { catalog: string | null; alive: boolean };
export type ExportAdmission = { operation: string; completion: Promise<Operation> };
type Ticket = {
  scope: Scope; previous: string | null; kind: OperationKind; job: string | null; failed: boolean;
  observed: string | null; resolve: (value: ExportAdmission) => void; reject: (reason: unknown) => void;
  complete?: (value: Operation) => void; interrupted?: (reason: unknown) => void;
};

/** One catalog-owned operation; status observation never replays an action. */
export function usePhotoExport(catalog: string | null) {
  const [operation, setOperation] = useState<Operation | null>(null), [ready, setReady] = useState(false);
  const [admitting, setAdmitting] = useState(false), [error, setError] = useState('');
  const scope = useRef<Scope>({ catalog, alive: false });
  const current = useRef<Operation | null>(null), ticket = useRef<Ticket | null>(null);
  const readEpoch = useRef(0), stopping = useRef<string | null>(null);
  const statusKnown = useRef(false);
  useEffect(() => {
    const context = { catalog, alive: true }; scope.current = context;
    const abort = new AbortController(); let timer: ReturnType<typeof setTimeout>;
    setOperation(null); current.current = null; statusKnown.current = false; setReady(false); setAdmitting(false); setError(''); stopping.current = null;
    const clearAdmission = (active: Ticket) => {
      if (ticket.current === active) { ticket.current = null; setAdmitting(false); }
    };
    const poll = async () => {
      const epoch = readEpoch.current;
      try {
        const value = await photoExport(catalog!, { command: 'status', args: { operation: null } }, 'operation', abort.signal);
        if (abort.signal.aborted || epoch !== readEpoch.current) return;
        if (JSON.stringify(value) !== JSON.stringify(current.current)) { current.current = value; setOperation(value); }
        statusKnown.current = true; setReady(true); setError('');
        const active = ticket.current;
        if (active?.scope === context) {
          if (!active.observed && value && value.id !== active.previous) {
            if (value.kind !== active.kind || (active.job !== null && value.job?.id !== active.job)) {
              active.reject(new Error('A different export operation became active. Review saved work before retrying.'));
              clearAdmission(active);
            } else if (active.failed) {
              // Caller already received its admission error. Global status still
              // exposes the accepted work without replaying that action.
              clearAdmission(active);
            } else {
              active.observed = value.id; setAdmitting(false);
              const completion = new Promise<Operation>((resolve, reject) => { active.complete = resolve; active.interrupted = reject; });
              // Run callers may only need global progress; unused completion
              // handles must not produce unhandled rejections on catalog Close.
              void completion.catch(() => {});
              active.resolve({ operation: value.id, completion });
            }
          } else if (!active.observed && active.failed) clearAdmission(active);
          if (active.observed) {
            if (!value || value.id !== active.observed) {
              active.interrupted?.(new Error('Export status was replaced. Inspect saved jobs and results.'));
              clearAdmission(active);
            } else if (terminal(value)) {
              active.complete?.(value); clearAdmission(active);
            }
          }
        }
        if (stopping.current && (value?.id !== stopping.current || terminal(value) || value.phase === 'cancel_requested')) stopping.current = null;
      } catch (e) {
        if (!abort.signal.aborted && epoch === readEpoch.current) { statusKnown.current = false; setReady(false); setError(errorText(e)); }
      } finally { if (!abort.signal.aborted) timer = setTimeout(() => void poll(), 500); }
    };
    if (catalog) void poll(); else setReady(true);
    return () => {
      context.alive = false; abort.abort(); clearTimeout(timer);
      if (ticket.current?.scope === context) {
        const reason = new Error('Catalog session changed; inspect saved export work after reopening.');
        ticket.current.reject(reason); ticket.current.interrupted?.(reason); ticket.current = null;
      }
    };
  }, [catalog]);
  type InactiveCancel = { command: 'cancel'; args: { job: string; operation: null } };
  const admit = async (request: ExportAction | InactiveCancel): Promise<ExportAdmission> => {
    const context = scope.current;
    if (!context.alive || context.catalog !== catalog) throw new Error('Catalog session changed; reopen the export controls.');
    if (!catalog || !statusKnown.current || ticket.current || current.current && (!terminal(current.current) || current.current.write_hold)) throw new Error('Wait for the current export operation or recover its status.');
    const job = 'job' in request.args ? request.args.job : null;
    readEpoch.current += 1;
    return new Promise<ExportAdmission>((resolve, reject) => {
      const active: Ticket = { scope: context, previous: current.current?.id ?? null, kind: request.command, job, failed: false, observed: null, resolve, reject };
      ticket.current = active; setAdmitting(true); setError('');
      void photoExport(catalog, request, request.command === 'cancel' ? 'job' : 'operation').then(value => {
        if (!value) throw new Error('Export operation was not admitted.');
      }).catch(e => {
        // Once independently observed, a delayed transport failure cannot revoke
        // a real operation or replace its current result.
        if (context.alive && scope.current === context && ticket.current === active && !active.observed) {
          readEpoch.current += 1; active.failed = true; statusKnown.current = false; setReady(false); setError(errorText(e)); reject(e);
        }
      });
    });
  };
  const stop = async (yielding: boolean) => {
    const target = current.current, context = scope.current;
    if (!context.alive || context.catalog !== catalog) throw new Error('Catalog session changed; reopen the export controls.');
    if (!catalog || !target || terminal(target) || stopping.current === target.id) return;
    if (yielding && (target.kind !== 'run' || !target.job)) throw new Error('Only a running photo export can yield.');
    stopping.current = target.id;
    try {
      await photoExport(catalog, yielding ? { command: 'yield', args: { job: target.job!.id, operation: target.id } } : { command: 'cancel', args: { job: target.job?.id ?? null, operation: target.id } }, 'operation');
    } catch (e) {
      if (context.alive && scope.current === context && stopping.current === target.id) { stopping.current = null; setError(errorText(e)); }
    }
  };
  const retry = () => {
    if (!scope.current.alive || scope.current.catalog !== catalog) return;
    // The serial poll loop keeps running after errors. Require its next fresh
    // result without destroying this catalog's admission or completion ticket.
    readEpoch.current += 1; statusKnown.current = false; setReady(false); setError('Rechecking export status…');
  };
  return { operation, ready, busy: !ready || admitting || !!operation && (!terminal(operation) || operation.write_hold), writeHeld: !ready || admitting || !!operation?.write_hold, error, admit: (request: ExportAction) => admit(request), cancelJob: (job: string) => admit({ command: 'cancel', args: { job, operation: null } }), cancel: () => stop(false), yield: () => stop(true), retry };
}

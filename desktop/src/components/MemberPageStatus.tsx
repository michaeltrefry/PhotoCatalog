import { ErrorNotice } from './Controls';

/** Exhaustion is a successful server result, never an inference from a failed load. */
export function MemberPageStatus({ loading, error, rows, scanned, hasNext }: { loading: boolean; error: string; rows: number; scanned: string; hasNext: boolean }) {
  if (error) return <><ErrorNotice message={error} /><p>Could not load this member page. Use First page / Refresh to try again.</p></>;
  if (loading) return <p role="status">Loading members…</p>;
  return <><p>{rows} members on this page · {scanned} candidates checked</p>{rows === 0 && <p>{hasNext ? 'More candidates remain. Continue to the next page.' : 'End of collection.'}</p>}</>;
}

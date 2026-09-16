import { describe, expect, it } from 'vitest';
import { renderToStaticMarkup } from 'react-dom/server';
import { MemberPageStatus } from './MemberPageStatus';
const render = (error: string, loading = false, hasNext = false) => renderToStaticMarkup(<MemberPageStatus loading={loading} error={error} rows={0} scanned="0" hasNext={hasNext} />);
describe('member page result states', () => {
  for (const error of ['Collection order index is preparing; retry after maintenance', 'Collection changed or cursor belongs to another collection', 'Organization member exceeds page bytes']) {
    it(`does not turn ${error} into empty collection evidence`, () => {
      const html = render(error);
      expect(html).toContain('role="alert"'); expect(html).toContain(error);
      expect(html).toContain('First page / Refresh'); expect(html).not.toContain('End of collection'); expect(html).not.toContain('0 members on this page');
    });
  }
  it('distinguishes loading, successful continuation, and successful exhaustion', () => {
    expect(render('', true)).toContain('Loading members'); expect(render('', true)).not.toContain('End of collection');
    expect(render('', false, true)).toContain('More candidates remain'); expect(render('', false, true)).not.toContain('End of collection');
    expect(render('')).toContain('End of collection');
  });
});

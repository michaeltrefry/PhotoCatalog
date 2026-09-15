import { describe, expect, it } from 'vitest';
import { budgetBytes, reviewedBudget } from './previewSettings';
describe('preview budget authority', () => {
  it('retains exact unedited byte budgets above JavaScript integer precision', () => {
    const current = '9007199254740993';
    expect(reviewedBudget((BigInt(current) / 1048576n).toString(), current)).toBe(current);
    expect(reviewedBudget('3', current)).toBe('3145728');
  });
  it('rejects fractional, noncanonical and excessive values', () => {
    for (const value of ['0', '-1', '1.5', '01', '9007199254740993']) expect(() => budgetBytes(value)).toThrow();
  });
});

import { expect, test, vi } from 'vitest';
import { PreviewCleanupCapacityError, PreviewCleanupCoordinator, type CleanupDisposition } from './previewCleanup';

type Failure = { kind: 'busy' | 'closed' | 'failure' };
const classify = (error: unknown): CleanupDisposition => {
  const kind = (error as Failure).kind;
  return kind === 'closed' ? 'retired' : kind;
};
const flush = async (turns = 8) => { for (let turn = 0; turn < turns; turn += 1) await Promise.resolve(); };
const reserveStarted = async (coordinator: PreviewCleanupCoordinator, catalog: string, viewport: string, generation: number) => {
  const lease = coordinator.reserve(catalog, viewport, String(generation));
  expect(await lease.ready).toBe(true);
  expect(lease.begin()).toBe(true);
  return lease;
};

test('serial cleanup drains a burst larger than the native control lane after typed Busy', async () => {
  const scheduled: (() => void)[] = [];
  const delivered: string[] = [];
  let pressure = 20;
  const coordinator = new PreviewCleanupCoordinator(
    async (_catalog, viewport) => {
      if (pressure-- > 0) throw { kind: 'busy' } satisfies Failure;
      delivered.push(viewport);
    },
    classify,
    run => { scheduled.push(run); },
  );
  const leases = await Promise.all(Array.from({ length: 32 }, (_, index) => reserveStarted(coordinator, 'catalog', `tile-${index}`, index + 1)));
  for (const lease of leases) { lease.settled(); lease.release(); }
  await flush();
  for (let retry = 0; retry < 20; retry += 1) {
    expect(scheduled).toHaveLength(1);
    scheduled.shift()!();
    await flush();
  }
  await flush(80);
  expect(delivered).toHaveLength(32);
  expect(new Set(delivered)).toEqual(new Set(Array.from({ length: 32 }, (_, index) => `tile-${index}`)));
  expect(coordinator.retained).toBe(0);
  expect(scheduled).toHaveLength(0);
});

test('coalesces only unstarted generations and never lets a stale release overtake the newest', async () => {
  let finishFirst!: () => void;
  const sent: string[] = [];
  const coordinator = new PreviewCleanupCoordinator(
    async (_catalog, _viewport, generation) => {
      sent.push(generation);
      if (generation === '1') await new Promise<void>(resolve => { finishFirst = resolve; });
    },
    classify,
  );
  const first = await reserveStarted(coordinator, 'catalog', 'tile', 1);
  first.settled(); first.release();
  await flush();
  const second = coordinator.reserve('catalog', 'tile', '2');
  const third = coordinator.reserve('catalog', 'tile', '3');
  await expect(second.ready).resolves.toBe(false);
  expect(sent).toEqual(['1']);
  finishFirst();
  expect(await third.ready).toBe(true);
  expect(third.begin()).toBe(true);
  await flush();
  expect(sent).toEqual(['1']);
  third.settled(); third.release();
  await flush();
  expect(sent).toEqual(['1', '3']);
  expect(coordinator.retained).toBe(0);
});

test('persistent Busy keeps one retry timer and rejects admission beyond reserved capacity', async () => {
  const scheduled: (() => void)[] = [];
  const coordinator = new PreviewCleanupCoordinator(
    async () => { throw { kind: 'busy' } satisfies Failure; },
    classify,
    run => { scheduled.push(run); },
    vi.fn(),
    2,
  );
  for (const [viewport, generation] of [['a', 1], ['b', 2]] as const) {
    const lease = await reserveStarted(coordinator, 'catalog', viewport, generation);
    lease.settled(); lease.release();
  }
  const refused = coordinator.reserve('catalog', 'c', '3');
  await expect(refused.ready).rejects.toBeInstanceOf(PreviewCleanupCapacityError);
  for (let retry = 0; retry < 40; retry += 1) {
    await flush();
    expect(scheduled).toHaveLength(1);
    expect(coordinator.retained).toBe(2);
    scheduled.shift()!();
  }
  await flush();
  expect(scheduled).toHaveLength(1);
  expect(coordinator.retained).toBe(2);
});

test('one persistently busy viewport cannot starve another eligible release', async () => {
  const scheduled: (() => void)[] = [];
  const sent: string[] = [];
  const coordinator = new PreviewCleanupCoordinator(
    async (_catalog, viewport) => {
      sent.push(viewport);
      if (viewport === 'blocked') throw { kind: 'busy' } satisfies Failure;
    },
    classify,
    run => { scheduled.push(run); },
  );
  const blocked = await reserveStarted(coordinator, 'catalog', 'blocked', 1);
  const available = await reserveStarted(coordinator, 'catalog', 'available', 2);
  blocked.settled(); blocked.release(); available.settled(); available.release();
  await flush();
  expect(sent).toEqual(['blocked']);
  scheduled.shift()!();
  await flush();
  expect(sent).toEqual(['blocked', 'available', 'blocked']);
  expect(coordinator.retained).toBe(1);
  expect(scheduled).toHaveLength(1);
});

test('a closed catalog retires its reservations without disturbing another catalog', async () => {
  const sent: string[] = [];
  const coordinator = new PreviewCleanupCoordinator(
    async (catalog, viewport) => {
      sent.push(`${catalog}:${viewport}`);
      if (catalog === 'closed') throw { kind: 'closed' } satisfies Failure;
    },
    classify,
  );
  const first = await reserveStarted(coordinator, 'closed', 'one', 1);
  const waiting = coordinator.reserve('closed', 'one', '2');
  const second = await reserveStarted(coordinator, 'closed', 'two', 3);
  first.settled(); first.release(); second.settled(); second.release();
  await expect(waiting.ready).resolves.toBe(false);
  await flush();
  expect(coordinator.retained).toBe(0);
  expect(sent).toEqual(['closed:one']);
  const live = await reserveStarted(coordinator, 'live', 'one', 4);
  live.settled(); live.release();
  await flush();
  expect(sent).toEqual(['closed:one', 'live:one']);
  expect(coordinator.retained).toBe(0);
});

test('teardown waits for late preview admission settlement before releasing', async () => {
  const send = vi.fn(async () => {});
  const coordinator = new PreviewCleanupCoordinator(send, classify);
  const lease = await reserveStarted(coordinator, 'catalog', 'tile', 1);
  lease.release();
  await flush();
  expect(send).not.toHaveBeenCalled();
  lease.settled();
  await flush();
  expect(send).toHaveBeenCalledWith('catalog', 'tile', '1');
  expect(coordinator.retained).toBe(0);
});

test('unexpected cleanup failure is retained, reported and surfaced to the next mount', async () => {
  const failure = { kind: 'failure' } satisfies Failure;
  const report = vi.fn();
  const coordinator = new PreviewCleanupCoordinator(async () => { throw failure; }, classify, undefined, report);
  const lease = await reserveStarted(coordinator, 'catalog', 'tile', 1);
  lease.settled(); lease.release();
  await flush();
  expect(report).toHaveBeenCalledWith(failure);
  expect(coordinator.retained).toBe(1);
  await expect(coordinator.reserve('catalog', 'tile', '2').ready).rejects.toBe(failure);
});

test('catalog activation retires a full failed catalog and old completion cannot lose new cleanup', async () => {
  let finishOld!: () => void;
  const sent: string[] = [];
  const coordinator = new PreviewCleanupCoordinator(
    async (catalog, viewport) => {
      sent.push(`${catalog}:${viewport}`);
      if (catalog === 'old' && viewport === 'old-127') await new Promise<void>(resolve => { finishOld = resolve; });
      else if (catalog === 'old') throw { kind: 'failure' } satisfies Failure;
    },
    classify,
    undefined,
    vi.fn(),
    128,
  );
  const old = await Promise.all(Array.from({ length: 128 }, (_, index) => reserveStarted(coordinator, 'old', `old-${index}`, index + 1)));
  for (const lease of old) { lease.settled(); lease.release(); }
  await flush(300);
  expect(sent.at(-1)).toBe('old:old-127');
  expect(coordinator.retained).toBe(128);

  const next = await reserveStarted(coordinator, 'new', 'new-one', 129);
  expect(coordinator.retained).toBe(1);
  next.settled(); next.release();
  await flush();
  expect(sent).not.toContain('new:new-one');
  finishOld();
  await flush();
  expect(sent.at(-1)).toBe('new:new-one');
  expect(coordinator.retained).toBe(0);
});

// The correctness harness's workload: random writes over a small pool of ids, so they collide, refer to each other
// and are refused often enough to exercise every rule
import fc from 'fast-check';
import type { Op, Value } from './model.ts';

export const IDS = ['Task-1', 'Task-2', 'Task-3', 'Task-4', 'Note-1', 'Note-2'] as const;
const KINDS = ['refs', 'blocks'] as const;
const KEYS = ['a', 'b', 'c'] as const;

const id = fc.constantFrom(...IDS);
const key = fc.constantFrom(...KEYS);
const kind = fc.constantFrom(...KINDS);
const value: fc.Arbitrary<Value> = fc.oneof(
  { weight: 4, arbitrary: fc.string({ maxLength: 8 }) },
  { weight: 4, arbitrary: fc.integer({ min: -100, max: 100 }) },
  { weight: 4, arbitrary: fc.array(fc.integer({ min: 0, max: 9 }), { maxLength: 3 }) },
  // Large enough to be kept out of memory once committed (OUT_OF_LINE_BYTES): a long string, and a long list an
  // append then grows
  { weight: 1, arbitrary: fc.integer({ min: 0, max: 9 }).map((n) => `${n}`.repeat(2100)) },
  { weight: 1, arbitrary: fc.integer({ min: 0, max: 9 }).map((n) => Array.from({ length: 1100 }, () => n)) },
);

/** One write that is not a unit */
const write: fc.Arbitrary<Op> = fc.oneof(
  { weight: 3, arbitrary: fc.record({ op: fc.constant('create' as const), id, fields: fc.dictionary(key, value, { maxKeys: 2 }) }) },
  { weight: 3, arbitrary: fc.record({ op: fc.constant('set' as const), id, key, value }) },
  { weight: 2, arbitrary: fc.record({ op: fc.constant('append' as const), id, key, value: fc.integer({ min: 0, max: 9 }) }) },
  { weight: 1, arbitrary: fc.record({ op: fc.constant('delete' as const), id, key }) },
  { weight: 3, arbitrary: fc.record({ op: fc.constant('link' as const), id, kind, target: id }) },
  { weight: 1, arbitrary: fc.record({ op: fc.constant('unlink' as const), id, kind, target: id }) },
  { weight: 1, arbitrary: fc.record({ op: fc.constant('destroy' as const), id }) },
);

/** Any op: a write, a unit of writes that returns or throws, or the store refusing the next commit */
export const op: fc.Arbitrary<Op> = fc.oneof(
  { weight: 8, arbitrary: write },
  { weight: 2, arbitrary: fc.record({ op: fc.constant('unit' as const), ops: fc.array(write, { minLength: 1, maxLength: 5 }), throws: fc.boolean(), durable: fc.boolean() }) },
  { weight: 1, arbitrary: fc.constant({ op: 'refuseNext' } as Op) },
);

export const workload = (maxLength = 40) => fc.array(op, { minLength: 1, maxLength });

/** A workload for a process that will be killed: no store refusals, which only a test's sink can inject */
export const crashWorkload = (length: number) =>
  fc.array(op.filter((each) => each.op !== 'refuseNext'), { minLength: length, maxLength: length });

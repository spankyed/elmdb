// The correctness harness's workload: random writes over a small pool of ids, so they collide, refer to each other
// and are refused often enough to exercise every rule
import fc from 'fast-check';
import { IDS, KINDS, ROLES, type Op, type Value } from './model.ts';

export { IDS };
const KEYS = ['a', 'b', 'c'] as const;

const id = fc.constantFrom(...IDS);
const key = fc.constantFrom(...KEYS);
const kind = fc.constantFrom(...KINDS);
const role = fc.constantFrom(...ROLES);
const mergeObj = fc.dictionary(fc.constantFrom('x', 'y'), fc.integer({ min: 0, max: 5 }), { minKeys: 1, maxKeys: 2 });
const value: fc.Arbitrary<Value> = fc.oneof(
  { weight: 3, arbitrary: fc.constantFrom('abc', 'ab', 'bca', 'axc', '5555', 'hello', '') },
  { weight: 2, arbitrary: fc.string({ maxLength: 8 }) },
  // Numbers from as small a pool as a list's elements, so an index lookup finds one often (`PROBES`)
  { weight: 4, arbitrary: fc.integer({ min: 0, max: 9 }) },
  { weight: 4, arbitrary: fc.array(fc.integer({ min: 0, max: 9 }), { maxLength: 3 }) },
  // Large values: a long string, and a long list an append then grows
  { weight: 1, arbitrary: fc.integer({ min: 0, max: 9 }).map((n) => `${n}`.repeat(2100)) },
  { weight: 1, arbitrary: fc.integer({ min: 0, max: 9 }).map((n) => Array.from({ length: 1100 }, () => n)) },
);

/** One write that is not a unit */
const write: fc.Arbitrary<Op> = fc.oneof(
  { weight: 3, arbitrary: fc.record({ op: fc.constant('create' as const), id, fields: fc.dictionary(key, value, { maxKeys: 2 }) }) },
  { weight: 2, arbitrary: fc.record({ op: fc.constant('upsert' as const), id, fields: fc.dictionary(key, value, { maxKeys: 2 }) }) },
  { weight: 3, arbitrary: fc.record({ op: fc.constant('set' as const), id, key, value }) },
  { weight: 1, arbitrary: fc.record({ op: fc.constant('merge' as const), id, key: fc.constant('c'), value: mergeObj }) },
  { weight: 2, arbitrary: fc.record({ op: fc.constant('append' as const), id, key, value: fc.integer({ min: 0, max: 9 }) }) },
  { weight: 1, arbitrary: fc.record({ op: fc.constant('delete' as const), id, key }) },
  { weight: 3, arbitrary: fc.record({ op: fc.constant('link' as const), id, kind, target: id }) },
  { weight: 1, arbitrary: fc.record({ op: fc.constant('unlink' as const), id, kind, target: id }) },
  { weight: 1, arbitrary: fc.record({ op: fc.constant('updateRelation' as const), id, kind, target: id, to: id }) },
  { weight: 1, arbitrary: fc.record({ op: fc.constant('removeRelation' as const), id, kind, target: id }) },
  { weight: 1, arbitrary: fc.record({ op: fc.constant('destroy' as const), id }) },
  { weight: 1, arbitrary: fc.record({ op: fc.constant('trash' as const), id }) },
  { weight: 1, arbitrary: fc.record({ op: fc.constant('restore' as const), id }) },
  { weight: 1, arbitrary: fc.record({ op: fc.constant('grant' as const), id, role }) },
  { weight: 1, arbitrary: fc.record({ op: fc.constant('grantExclusive' as const), id, role }) },
  { weight: 1, arbitrary: fc.record({ op: fc.constant('revoke' as const), id, role }) },
);

const unitOf = (ops: fc.Arbitrary<Op>) =>
  fc.record({ op: fc.constant('unit' as const), ops: fc.array(ops, { minLength: 1, maxLength: 5 }), throws: fc.boolean(), durable: fc.boolean() });

/** A unit's writes, one of them sometimes a unit of its own: a savepoint, which rolls back alone */
const unitWrite: fc.Arbitrary<Op> = fc.oneof({ weight: 4, arbitrary: write }, { weight: 1, arbitrary: unitOf(write) });

/** Any op: a write, or a unit of writes that returns or throws */
export const op: fc.Arbitrary<Op> = fc.oneof(
  { weight: 8, arbitrary: write },
  { weight: 2, arbitrary: unitOf(unitWrite) },
);

/**
 * Every id created first, with fields of its own, so what follows lands on data: without it a run averaged six ops
 * over less than one entity, and refused nearly all of them (not one link in 300 runs). Destroying and creating
 * again still happen, and refusals with them
 */
const prelude: fc.Arbitrary<Op[]> = fc.tuple(...IDS.map((each) => fc.dictionary(key, value, { maxKeys: 2 })
  .map((fields): Op => ({ op: 'create', id: each, fields }))));

/**
 * An edge linked, unlinked and linked again, in a row: what a stale key between two ends answers wrongly, and which
 * independent ops over 60 possible edges almost never make
 */
const relink: fc.Arbitrary<Op[]> = fc.record({ id, kind, target: id })
  .map(({ id: source, kind: k, target }): Op[] => [
    { op: 'link', id: source, kind: k, target },
    { op: 'unlink', id: source, kind: k, target },
    { op: 'link', id: source, kind: k, target },
  ]);

/** The ops after the prelude: single ops, and now and then a relink */
const ops = (minLength: number, maxLength: number, each: fc.Arbitrary<Op>) =>
  fc.array(fc.oneof({ weight: 9, arbitrary: each.map((one) => [one]) }, { weight: 1, arbitrary: relink }), { minLength, maxLength })
    .map((chunks) => chunks.flat());

/** A workload of `length` ops, the prelude's first */
export const workload = (length: number) =>
  fc.tuple(prelude, ops(length, length, op)).map(([first, rest]) => [...first, ...rest].slice(0, length));

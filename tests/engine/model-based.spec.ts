// The correctness harness, model-based: random workloads (tests/_harness/) run against the engine and a reference
// model. After every write the engine holds what the model holds and refuses what it refuses, a refused unit
// included; and a unit that rolls back leaves each type in its insertion order. On its own memory store, whose
// backend refuses a commit on demand, so it runs in the `ears` project only. The files' half is
// tests/lmdb/crash.integration.spec.ts
import fc from 'fast-check';
import { describe, expect, it } from 'vitest';
import { createMemoryKVBackend, createOrderedKV } from '../../src/kv.ts';
import { Model, applyTo, stateOf } from '../_harness/model.ts';
import { newEngine, refusable } from '../_harness/engines.ts';
import { workload } from '../_harness/workload.ts';

/** An engine on a memory store that refuses its next commit when told to */
const refusingEngine = () => {
  const { backend, refuseNext } = refusable(createMemoryKVBackend());
  return { engine: newEngine(createOrderedKV(backend)), refuseNext };
};

describe('the engine against the model', () => {
  it('holds what the model holds after every write, and refuses what it refuses', () => {
    fc.assert(fc.property(workload(), (ops) => {
      const { engine, refuseNext } = refusingEngine();
      const model = new Model();
      for (const op of ops) {
        const refused = applyTo(engine, op, refuseNext);
        expect(refused, JSON.stringify(op)).toBe(model.apply(op) !== undefined);
        expect(stateOf(engine)).toEqual(model.state());
      }
    }), { numRuns: 300 });
  });

  it('keeps each type in insertion order through a unit that rolls back', () => {
    fc.assert(fc.property(workload(), (ops) => {
      const { engine, refuseNext } = refusingEngine();
      for (const op of ops) {
        const before = engine.query('Task').ids();
        if (applyTo(engine, op, refuseNext)) expect(engine.query('Task').ids(), JSON.stringify(op)).toEqual(before);
      }
    }), { numRuns: 300 });
  });
});

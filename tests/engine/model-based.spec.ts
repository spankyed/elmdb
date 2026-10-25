// The correctness harness, model-based: random workloads (tests/_harness/) run against the engine and a reference
// model. After every write the engine holds what the model holds and refuses what it refuses, a refused unit and a
// rolled-back savepoint included, and every derived read — a type's entities, count and trash, an index lookup, a
// role, an edge's far ends — answers as the model does; and a unit that rolls back leaves each type in its insertion
// order; and a watch on every one of those reads delivers what the read answers afresh, after every op. On a memory
// backend and an LMDB one, each refusing a commit on demand, so it builds its own engines and runs
// in the `ears` project only. A killed writer's files are tests/lmdb/crash.integration.spec.ts
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import fc from 'fast-check';
import { afterAll, describe, expect, it } from 'vitest';
import { createMemoryKVBackend, createOrderedKV, type KvBackend } from '../../src/kv.ts';
import { createLmdbKVBackend, openEnvironment } from '../../src/lmdb/index.ts';
import { Model, applyTo, derivedOf, stateOf, watchedReads, type Op } from '../_harness/model.ts';
import { newEngine, refusable } from '../_harness/engines.ts';
import { workload } from '../_harness/workload.ts';

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'ears-model-'));
afterAll(() => fs.rmSync(root, { recursive: true, force: true }));

/** A backend for one run, and how to give it back */
interface Backing { backend: KvBackend; close(): void }

const memory = (): Backing => ({ backend: createMemoryKVBackend(), close: () => {} });

let run = 0;
/** An LMDB environment of its own, with the seam's backend over it; what a commit's sync costs is not the subject */
const lmdb = (): Backing => {
  const dir = path.join(root, `run-${run++}`, 'ears-db');
  const { root: env, db } = openEnvironment(dir);
  return {
    backend: createLmdbKVBackend(env, db, { committed() {}, synced() {}, durable: () => Promise.resolve() }),
    close: () => { env.close(); fs.rmSync(path.dirname(dir), { recursive: true, force: true }); },
  };
};

/**
 * An engine on the backing, and `apply`: an op applied to the engine and the model, the backend refusing a commit
 * while the op runs exactly when the model expects the store to refuse one. Whether the engine refused, and whether
 * the model did
 */
const harness = (backing: Backing) => {
  const { backend, refusing } = refusable(backing.backend);
  const engine = newEngine(createOrderedKV(backend));
  const model = new Model();
  const apply = (op: Op) => {
    refusing(model.refusing);
    const refused = applyTo(engine, op);
    refusing(false);
    return { refused, expected: model.apply(op) !== undefined };
  };
  return { engine, model, apply };
};

describe.each([
  { name: 'a memory backend', backing: memory, runs: 300 },
  { name: 'an LMDB backend', backing: lmdb, runs: 60 },
])('the engine against the model, on $name', ({ backing, runs }) => {
  it('holds what the model holds after every write, refuses what it refuses, and answers every derived read as it does', () => {
    fc.assert(fc.property(workload(), (ops) => {
      const store = backing();
      try {
        const { engine, model, apply } = harness(store);
        for (const op of ops) {
          const { refused, expected } = apply(op);
          expect(refused, JSON.stringify(op)).toBe(expected);
          expect(stateOf(engine), JSON.stringify(op)).toEqual(model.state());
          expect(derivedOf(engine, model), JSON.stringify(op)).toEqual(model.derived());
        }
      } finally {
        store.close();
      }
    }), { numRuns: runs });
  });

  it('keeps every watch at what its read answers afresh, after every write, whatever wrote', async () => {
    await fc.assert(fc.asyncProperty(workload(), async (ops) => {
      const store = backing();
      const stops: Array<() => void> = [];
      try {
        const { engine, apply } = harness(store);
        const latest = new Map<string, unknown>();
        for (const [name, read] of Object.entries(watchedReads(engine, ops))) {
          const watching = engine.watch(read, {
            next: (value) => { latest.set(name, value); },
            error: (error) => { latest.set(name, { threw: String(error) }); },
          });
          stops.push(() => watching.stop());
        }
        const reads = watchedReads(engine, ops);
        for (const op of ops) {
          apply(op);
          await engine.admin.watchesSettled();
          for (const [name, read] of Object.entries(reads)) {
            let fresh: unknown;
            try { fresh = read(); } catch (error) { fresh = { threw: String(error) }; }
            expect(latest.get(name), `${name} after ${JSON.stringify(op)}`).toEqual(fresh);
          }
        }
      } finally {
        for (const stop of stops) stop();
        store.close();
      }
    }), { numRuns: Math.ceil(runs / 3) });
  });

  it('keeps each type in insertion order through a unit that rolls back', () => {
    fc.assert(fc.property(workload(), (ops) => {
      const store = backing();
      try {
        const { engine, apply } = harness(store);
        for (const op of ops) {
          const before = engine.query('Task').ids();
          if (apply(op).refused) expect(engine.query('Task').ids(), JSON.stringify(op)).toEqual(before);
        }
      } finally {
        store.close();
      }
    }), { numRuns: runs });
  });
});

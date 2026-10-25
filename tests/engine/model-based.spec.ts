// The correctness harness, model-based: random workloads (tests/_harness/) run against the engine on elmdb and a
// reference model. After every op the engine refuses what the model refuses and holds what it holds, a rolled-back unit
// and savepoint included, and every derived read — a type's entities, count and trash, an index lookup, a role, an
// edge's far ends — answers as the model does. A store refusing a commit is no op here, since nothing in front of
// elmdb can inject one: elmdb's own model harness covers that (elmdb/crates/elmdb/src/engine/tests/model.rs)
import fc from 'fast-check';
import { afterAll, expect, it } from 'vitest';
import { createEarsEngine } from '../../src/index.ts';
import { createTemporaryDatabases } from '../../src/lmdb/index.ts';
import { INDEXED, KINDS, Model, ROLES, TYPES, applyTo, derivedOf, stateOf, type Op } from '../_harness/model.ts';
import { workload } from '../_harness/workload.ts';

const databases = createTemporaryDatabases();
afterAll(() => databases.disposeAll());

/** The engine over the harness's names, every type indexing the field the workload probes */
function harnessEngine() {
  const { database, dispose } = databases.open({
    types: Object.fromEntries(TYPES.map((type) => [type, { required: [], indexes: [INDEXED] }])),
    relations: Object.fromEntries(KINDS.map((kind) => [kind, {}])),
    roles: [...ROLES],
  });
  return { engine: createEarsEngine({ database }), dispose };
}

it('refuses, holds and answers what the model does', () => {
  fc.assert(fc.property(fc.integer({ min: 10, max: 60 }).chain((length) => workload(length)), (ops: Op[]) => {
    const { engine, dispose } = harnessEngine();
    const model = new Model();
    try {
      for (const op of ops) {
        const where = JSON.stringify(op);
        expect(applyTo(engine, op), where).toBe(model.apply(op) !== undefined);
        expect(stateOf(engine), where).toEqual(model.state());
        expect(derivedOf(engine, model), where).toEqual(model.derived());
      }
    } finally {
      dispose();
    }
  }), { numRuns: 120 });
});

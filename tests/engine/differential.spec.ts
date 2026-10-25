// The differential harness: one random workload (tests/_harness/) driving the JavaScript engine on its memory store,
// the elmdb engine, and the reference model. After every write the two engines refuse what the model refuses, hold
// what it holds and answer every derived read as it does, and agree with each other. A store refusing a commit is no
// op here, since only the JavaScript engine's backend can inject one: elmdb's own model harness covers that
// (elmdb/crates/elmdb/src/engine/tests/model.rs).
import fc from 'fast-check';
import { afterAll, expect, it } from 'vitest';
import { createEarsEngine } from '../../src/index.ts';
import { createTemporaryDatabases } from '../../src/lmdb/index.ts';
import { createMemoryKV } from '../../src/kv.ts';
import { INDEXED, KINDS, Model, ROLES, TYPES, applyTo, derivedOf, stateOf, type Op } from '../_harness/model.ts';
import { newEngine } from '../_harness/engines.ts';
import { crashWorkload } from '../_harness/workload.ts';

const databases = createTemporaryDatabases();
afterAll(() => databases.disposeAll());

/** The elmdb engine over the harness's names, every type indexing the field the workload probes */
function elmdbEngine() {
  const { database, dispose } = databases.open({
    types: Object.fromEntries(TYPES.map((type) => [type, { required: [], indexes: [INDEXED] }])),
    relations: Object.fromEntries(KINDS.map((kind) => [kind, {}])),
    roles: [...ROLES],
  });
  return { engine: createEarsEngine({ database }), dispose };
}

it('the JavaScript engine and the elmdb engine refuse, hold and answer what the model does, and agree with each other', () => {
  fc.assert(fc.property(fc.integer({ min: 10, max: 60 }).chain((length) => crashWorkload(length)), (ops: Op[]) => {
    const kv = newEngine(createMemoryKV());
    const { engine: elmdb, dispose } = elmdbEngine();
    const model = new Model();
    try {
      for (const op of ops) {
        const where = JSON.stringify(op);
        const refusedByKv = applyTo(kv, op);
        const refusedByElmdb = applyTo(elmdb, op);
        const expected = model.apply(op) !== undefined;
        expect({ kv: refusedByKv, elmdb: refusedByElmdb }, where).toEqual({ kv: expected, elmdb: expected });
        const state = model.state();
        const derived = model.derived();
        expect(stateOf(elmdb), where).toEqual(state);
        expect(stateOf(kv), where).toEqual(state);
        expect(stateOf(elmdb), where).toEqual(stateOf(kv));
        expect(derivedOf(elmdb, model), where).toEqual(derived);
        expect(derivedOf(kv, model), where).toEqual(derived);
        expect(derivedOf(elmdb, model), where).toEqual(derivedOf(kv, model));
      }
    } finally {
      dispose();
    }
  }), { numRuns: 120 });
});

// The correctness harness, model-based: random workloads (tests/_harness/) run against the engine on elmdb and a
// reference model. After every op the engine refuses what the model refuses and holds what it holds, a rolled-back unit
// and savepoint included, and every derived read — a type's entities, count and trash, an index lookup, a role, an
// edge's far ends — answers as the model does; and a watch on every one of those reads delivers what the read answers
// afresh, after every op. A store refusing a commit is no op here, since nothing in front of
// elmdb can inject one: elmdb's own model harness covers that (elmdb/crates/elmdb/src/engine/tests/model.rs)
import fc from 'fast-check';
import { afterAll, beforeAll, expect, it } from 'vitest';
import { createEarsEngine } from '../../src/index.ts';
import { openDatabase } from '../../src/lmdb/index.ts';
import { copyFileSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { INDEXED, KINDS, Model, ROLES, TYPES, applyTo, derivedOf, stateOf, watchedReads, type Op } from '../_harness/model.ts';
import { workload } from '../_harness/workload.ts';

let directory: string | undefined;
const disposals = new Set<() => void>();
const disposeAll = () => {
  for (const dispose of disposals) dispose();
  if (directory) rmSync(directory, { recursive: true, force: true });
};
process.once('exit', disposeAll);
afterAll(() => { process.off('exit', disposeAll); disposeAll(); });
beforeAll(() => {
  directory = mkdtempSync(join(tmpdir(), 'ears-model-'));
  const seed = openDatabase({ path: join(directory, 'source'), syncs: false, schema: {
    types: Object.fromEntries(TYPES.map((type) => [type, {
      required: [],
      indexes: [INDEXED],
      search: { fields: { a: 2, b: 1 } },
      substrings: ['a', 'b'],
      embed: { from: ['a', 'b'], model: 'test-v1', dims: 3 },
    }])),
    relations: Object.fromEntries(KINDS.map((kind) => [kind, {}])),
    roles: [...ROLES],
    ranks: { modelRank: { types: [...TYPES], relations: [...KINDS], direction: 'both' } },
  } });
  // Add a compound value index (`['b', 'a']`) on Task so watched compound-index plans run against a real index
  const withCompound = structuredClone(seed.schema!);
  const taskType = withCompound.types!.Task!;
  taskType.indexes = {
    ...taskType.indexes,
    ba: { kind: 'value', fields: ['b', 'a'] },
  };
  seed.setSchema(withCompound);
  seed.waitForIndexBuilds();
  try { seed.backup(join(directory, 'seed')); } finally { seed.close(); }
});

/** Each workload gets independent files and a native handle; only empty, closed bootstrap data is copied. */
function harnessEngine() {
  const own = mkdtempSync(join(directory!, 'case-'));
  copyFileSync(join(directory!, 'seed', 'data.mdb'), join(own, 'data.mdb'));
  const database = openDatabase({ path: own, syncs: false });
  const dispose = () => {
    disposals.delete(dispose);
    if (database.isOpen) database.close();
    rmSync(own, { recursive: true, force: true });
  };
  disposals.add(dispose);
  try { return { engine: createEarsEngine({ database }), dispose }; }
  catch (error) { dispose(); throw error; }
}

it('refuses, holds and answers what the model does', () => {
  fc.assert(fc.property(fc.integer({ min: 10, max: 60 }).chain((length) => workload(length)), (ops: Op[]) => {
    const { engine, dispose } = harnessEngine();
    const model = new Model();
    try {
      expect(stateOf(engine)).toEqual(model.state());
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

it('keeps every watch at what its read answers afresh, after every write, whatever wrote', async () => {
  await fc.assert(fc.asyncProperty(fc.integer({ min: 10, max: 60 }).chain((length) => workload(length)), async (ops: Op[]) => {
    const { engine, dispose } = harnessEngine();
    const stops: Array<() => void> = [];
    const watchOpts = { search: true, substrings: true, compound: true, ranked: 'modelRank' } as const;
    try {
      expect(engine.query.withTrashed.all().exists()).toBe(false);
      // Confirm the compound value index is active on Task
      expect(
        engine.query('Task').where('b', 3).orderBy('a', 'asc').limit(2).explain().stages
          .some((stage) => stage.index === 'ba'),
      ).toBe(true);
      const latest = new Map<string, unknown>();
      const reads = watchedReads(engine, ops, watchOpts);
      for (const [name, read] of Object.entries(reads)) {
        const watching = engine.watch(read, {
          next: (value) => { latest.set(name, value); },
          error: (error) => { latest.set(name, { threw: String(error) }); },
        });
        stops.push(() => watching.stop());
      }
      for (const op of ops) {
        applyTo(engine, op);
        engine.admin.waitForIndexBuilds();
        await engine.admin.watchesSettled();
        for (const [name, read] of Object.entries(reads)) {
          let fresh: unknown;
          try { fresh = read(); } catch (error) { fresh = { threw: String(error) }; }
          expect(latest.get(name), `${name} after ${JSON.stringify(op)}`).toEqual(fresh);
        }
      }
    } finally {
      for (const stop of stops) stop();
      dispose();
    }
  }), { numRuns: 25 });
});

it('keeps vector near/like/hybrid watches converged across writes and embed() and emits zero notifications on rolled-back hooks/transacts', async () => {
  const { engine, dispose } = harnessEngine();
  const stops: Array<() => void> = [];
  let rejectHook = false;
  engine.admin.beforeCommit({
    wants: (type) => type === 'Task' || type === 'Note',
    run() {
      if (rejectHook) throw new Error('rolled-back beforeCommit');
    },
  });
  try {
    const vecOf = (text: string): [number, number, number] => {
      if (text.includes('alpha')) return [1, 0, 0];
      if (text.includes('beta')) return [0, 1, 0];
      return [0, 0, 1];
    };
    const embedPending = () => {
      for (const type of engine.embeddedTypes()) {
        const pending = engine.pendingEmbeddings(type);
        if (pending.chunks.length > 0) {
          engine.embed(pending.chunks.map((c) => ({
            id: c.id, chunk: c.chunk, hash: c.hash, model: pending.model, vector: vecOf(c.text),
          })));
        }
      }
    };

    const reads: Record<string, () => unknown> = {
      nearAlpha: () => engine.query('Task').near([1, 0, 0], { k: 3 }).hits(),
      nearBeta: () => engine.query('Task').near([0, 1, 0], { k: 3 }).ids(),
      likeTask1: () => engine.query('Task').like('Task-1' as never, { k: 3 }).ids(),
      hybridAlpha: () => engine.query('Task').hybrid('alpha', [1, 0, 0], { k: 3 }).hits(),
      rankedTasks: () => engine.query('Task').ranked('modelRank').limit(3).ids(),
    };
    const latest = new Map<string, unknown>();
    let totalRuns = 0;
    let totalDeliveries = 0;
    for (const [name, read] of Object.entries(reads)) {
      const watching = engine.watch(() => {
        totalRuns++;
        return read();
      }, {
        next: (value) => { totalDeliveries++; latest.set(name, value); },
        error: (error) => { totalDeliveries++; latest.set(name, { threw: String(error) }); },
      });
      stops.push(() => watching.stop());
    }
    const assertOracle = async (label: string) => {
      engine.admin.waitForIndexBuilds();
      await engine.admin.watchesSettled();
      for (const [name, read] of Object.entries(reads)) {
        let fresh: unknown;
        try { fresh = read(); } catch (error) { fresh = { threw: String(error) }; }
        expect(latest.get(name), `${name} after ${label}`).toEqual(fresh);
      }
    };

    await assertOracle('initial');
    engine.create('Task', { id: 'Task-1' as never, a: 'alpha one', b: 3 });
    engine.create('Task', { id: 'Task-2' as never, a: 'beta two', b: 3 });
    engine.create('Task', { id: 'Task-3' as never, a: 'gamma three', b: 3 });
    engine.write('Task-2' as never).link('refs', 'Task-1' as never);
    await assertOracle('creates');

    embedPending();
    await assertOracle('first embed');

    engine.write('Task-3' as never).set('a', 'alpha updated');
    embedPending();
    await assertOracle('update + re-embed');

    // Verify rolled-back beforeCommit hooks and aborted transacts trigger ZERO watch runs, deliveries, or sequence advance
    const seqBefore = engine.lastCommit();
    const runsBefore = totalRuns;
    const deliveriesBefore = totalDeliveries;

    expect(() => engine.transact(() => {
      engine.write('Task-1' as never).set('a', 'beta aborted');
      engine.write('Task-2' as never).trash();
      throw new Error('aborted transact');
    })).toThrow('aborted transact');

    rejectHook = true;
    expect(() => engine.write('Task-1' as never).set('a', 'hook rejected')).toThrow('rolled-back beforeCommit');
    expect(() => engine.transact(() => {
      engine.create('Task', { id: 'Task-4' as never, a: 'alpha hook rejected' });
    })).toThrow('rolled-back beforeCommit');
    rejectHook = false;

    await engine.admin.watchesSettled();
    expect(engine.lastCommit(), 'sequence did not advance on rollback').toBe(seqBefore);
    expect(totalRuns, 'zero watch runs on rollback').toBe(runsBefore);
    expect(totalDeliveries, 'zero watch deliveries on rollback').toBe(deliveriesBefore);

    // Trash, restore, destroy all converge vector & ranked watches
    engine.write('Task-1' as never).trash();
    await assertOracle('trash Task-1');
    engine.write('Task-1' as never).restore();
    await assertOracle('restore Task-1');
    engine.write('Task-3' as never).destroy();
    await assertOracle('destroy Task-3');
  } finally {
    for (const stop of stops) stop();
    dispose();
  }
});

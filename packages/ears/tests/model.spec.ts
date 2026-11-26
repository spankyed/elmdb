import fc from 'fast-check';
import { afterEach, describe, expect, it } from 'vitest';
import { createEarsEngine, type EarsEngine, type Schema } from '../src/index.ts';
import { createTemporaryDatabases } from '../src/lmdb/index.ts';

const modelSchema: Schema = {
  types: {
    Item: {
      fields: {
        name: { type: 'string' },
        rank: { type: 'integer' },
        tag: { type: 'string', optional: true },
      },
      indexes: {
        byRank: { kind: 'value', fields: ['rank'] },
        byTag: { kind: 'value', fields: ['tag'] },
      },
    },
  },
  relations: {
    links: {
      cardinality: 'manyToMany',
      endpoints: [{ source: 'Item', target: 'Item' }],
    },
  },
};

interface ModelEntity {
  id: string;
  name: string;
  rank: number;
  tag?: string;
  trashed: boolean;
}

interface OracleState {
  entities: Map<string, ModelEntity>;
  edges: Set<string>; // `${src}\0${dst}`
  counters: Map<string, number>;
}

function cloneOracle(state: OracleState): OracleState {
  const entities = new Map<string, ModelEntity>();
  for (const [k, v] of state.entities) {
    entities.set(k, { ...v });
  }
  return {
    entities,
    edges: new Set(state.edges),
    counters: new Map(state.counters),
  };
}

function assertMatchesOracle(engine: EarsEngine, oracle: OracleState): void {
  // 1. Point lookups & edge adjacency
  for (const id of IDS) {
    const expected = oracle.entities.get(id);
    const actual = engine.get(id);
    if (!expected) {
      expect(actual).toBeNull();
    } else {
      expect(actual).not.toBeNull();
      expect(actual?.trashed).toBe(expected.trashed);
      expect(actual?.fields['name']).toBe(expected.name);
      expect(actual?.fields['rank']).toBe(expected.rank);
      expect(actual?.fields['tag']).toBe(expected.tag);
    }

    const expectedOut = [...oracle.edges]
      .filter((e) => e.startsWith(`${id}\0`))
      .map((e) => e.split('\0')[1]!)
      .sort();
    const actualOut = expected
      ? engine
          .getEdges(id, { relKind: 'links', dir: 'out' })
          .map((e) => e.targetId)
          .sort()
      : [];
    expect(actualOut).toEqual(expectedOut);
  }

  // 2. Counters
  for (const name of COUNTER_NAMES) {
    expect(engine.getCounter(name)).toBe(oracle.counters.get(name) ?? 0);
  }

  // 3. Indexed queries (live only + includeTrashed + value index filter + count)
  const expectedLiveIds = [...oracle.entities.values()]
    .filter((e) => !e.trashed)
    .map((e) => e.id)
    .sort();
  expect(engine.query.of('Item').ids().sort()).toEqual(expectedLiveIds);
  expect(engine.query.of('Item').count()).toBe(expectedLiveIds.length);

  const expectedAllIds = [...oracle.entities.values()].map((e) => e.id).sort();
  expect(engine.query.of('Item', { includeTrashed: true }).ids().sort()).toEqual(expectedAllIds);

  const expectedRank2 = [...oracle.entities.values()]
    .filter((e) => !e.trashed && e.rank === 2)
    .map((e) => e.id)
    .sort();
  expect(engine.query.of('Item').where('rank', 2).ids().sort()).toEqual(expectedRank2);
  expect(engine.query.of('Item').where('rank', 2).count()).toBe(expectedRank2.length);
}

const IDS = ['i0', 'i1', 'i2', 'i3', 'i4', 'i5'] as const;
const TAGS = ['alpha', 'beta', 'gamma'] as const;
const COUNTER_NAMES = ['DOC', 'TASK', 'SEQ'] as const;

type AtomicStep =
  | { kind: 'create'; id: string; name: string; rank: number; tag?: string }
  | { kind: 'patch'; id: string; rank: number; tag?: string }
  | { kind: 'unsetTag'; id: string }
  | { kind: 'trash'; id: string }
  | { kind: 'restore'; id: string }
  | { kind: 'destroy'; id: string }
  | { kind: 'link'; src: string; dst: string }
  | { kind: 'unlink'; src: string; dst: string }
  | { kind: 'nextCounter'; name: string }
  | { kind: 'setCounter'; name: string; value: number };

type TopStep =
  | { kind: 'atomic'; step: AtomicStep }
  | {
      kind: 'transact';
      outerBefore: AtomicStep[];
      inner: AtomicStep[];
      abortInner: boolean;
      outerAfter: AtomicStep[];
      abortOuter: boolean;
      runBeforeCommit: boolean;
    };

const idArb = fc.constantFrom(...IDS);
const tagArb = fc.option(fc.constantFrom(...TAGS), { nil: undefined });
const counterArb = fc.constantFrom(...COUNTER_NAMES);

const atomicStepArb: fc.Arbitrary<AtomicStep> = fc.oneof(
  fc.record({
    kind: fc.constant('create' as const),
    id: idArb,
    name: fc.constantFrom('one', 'two', 'three', 'four'),
    rank: fc.integer({ min: 0, max: 4 }),
    tag: tagArb,
  }),
  fc.record({
    kind: fc.constant('patch' as const),
    id: idArb,
    rank: fc.integer({ min: 0, max: 4 }),
    tag: tagArb,
  }),
  fc.record({ kind: fc.constant('unsetTag' as const), id: idArb }),
  fc.record({ kind: fc.constant('trash' as const), id: idArb }),
  fc.record({ kind: fc.constant('restore' as const), id: idArb }),
  fc.record({ kind: fc.constant('destroy' as const), id: idArb }),
  fc.record({ kind: fc.constant('link' as const), src: idArb, dst: idArb }),
  fc.record({ kind: fc.constant('unlink' as const), src: idArb, dst: idArb }),
  fc.record({ kind: fc.constant('nextCounter' as const), name: counterArb }),
  fc.record({
    kind: fc.constant('setCounter' as const),
    name: counterArb,
    value: fc.integer({ min: 0, max: 50 }),
  }),
);

const topStepArb: fc.Arbitrary<TopStep> = fc.oneof(
  { weight: 3, arbitrary: atomicStepArb.map((step): TopStep => ({ kind: 'atomic', step })) },
  {
    weight: 2,
    arbitrary: fc.record({
      kind: fc.constant('transact' as const),
      outerBefore: fc.array(atomicStepArb, { minLength: 0, maxLength: 3 }),
      inner: fc.array(atomicStepArb, { minLength: 0, maxLength: 3 }),
      abortInner: fc.boolean(),
      outerAfter: fc.array(atomicStepArb, { minLength: 0, maxLength: 2 }),
      abortOuter: fc.boolean(),
      runBeforeCommit: fc.boolean(),
    }),
  },
);

function applyAtomicStep(engine: EarsEngine, oracle: OracleState, step: AtomicStep): void {
  switch (step.kind) {
    case 'create': {
      if (oracle.entities.has(step.id)) return;
      const fields: Record<string, unknown> = { name: step.name, rank: step.rank };
      if (step.tag !== undefined) fields['tag'] = step.tag;
      engine.create('Item', fields, step.id);
      oracle.entities.set(step.id, {
        id: step.id,
        name: step.name,
        rank: step.rank,
        ...(step.tag !== undefined && { tag: step.tag }),
        trashed: false,
      });
      return;
    }
    case 'patch': {
      const cur = oracle.entities.get(step.id);
      if (!cur || cur.trashed) return;
      const fields: Record<string, unknown> = { rank: step.rank };
      if (step.tag !== undefined) fields['tag'] = step.tag;
      engine.patch(step.id, fields);
      cur.rank = step.rank;
      if (step.tag !== undefined) cur.tag = step.tag;
      return;
    }
    case 'unsetTag': {
      const cur = oracle.entities.get(step.id);
      if (!cur || cur.trashed) return;
      engine.unset(step.id, ['tag']);
      delete cur.tag;
      return;
    }
    case 'trash': {
      const cur = oracle.entities.get(step.id);
      if (!cur || cur.trashed) return;
      engine.trash(step.id);
      cur.trashed = true;
      return;
    }
    case 'restore': {
      const cur = oracle.entities.get(step.id);
      if (!cur || !cur.trashed) return;
      engine.restore(step.id);
      cur.trashed = false;
      return;
    }
    case 'destroy': {
      if (!oracle.entities.has(step.id)) return;
      engine.destroy(step.id);
      oracle.entities.delete(step.id);
      for (const edge of [...oracle.edges]) {
        const [s, d] = edge.split('\0');
        if (s === step.id || d === step.id) oracle.edges.delete(edge);
      }
      return;
    }
    case 'link': {
      const src = oracle.entities.get(step.src);
      const dst = oracle.entities.get(step.dst);
      if (!src || !dst || src.trashed || dst.trashed) return;
      const key = `${step.src}\0${step.dst}`;
      if (oracle.edges.has(key)) return;
      engine.link(step.src, 'links', step.dst);
      oracle.edges.add(key);
      return;
    }
    case 'unlink': {
      const src = oracle.entities.get(step.src);
      if (!src) return;
      const key = `${step.src}\0${step.dst}`;
      if (!oracle.edges.has(key)) return;
      engine.unlink(step.src, 'links', step.dst);
      oracle.edges.delete(key);
      return;
    }
    case 'nextCounter': {
      const next = (oracle.counters.get(step.name) ?? 0) + 1;
      expect(engine.nextCounter(step.name)).toBe(next);
      oracle.counters.set(step.name, next);
      return;
    }
    case 'setCounter': {
      engine.setCounter(step.name, step.value);
      oracle.counters.set(step.name, step.value);
      return;
    }
  }
}

const temps = createTemporaryDatabases();
afterEach(() => {
  temps.disposeAll();
});

describe('@elmdb/ears fast-check model-based oracle suite', () => {
  it('matches reference oracle across randomized CRUD, links, nested transact savepoints, counters, and hooks', () => {
    fc.assert(
      fc.property(fc.array(topStepArb, { minLength: 5, maxLength: 28 }), (steps) => {
        const handle = temps.open({ schema: modelSchema, buildPolicy: 'eager' });
        const engine = createEarsEngine({ database: handle.database });
        let oracle: OracleState = {
          entities: new Map(),
          edges: new Set(),
          counters: new Map(),
        };

        // Track outermost beforeCommit invocations (only fires on commits where hooks ran)
        let hookInvocations = 0;
        const unsub = engine.beforeCommit(null, (delta) => {
          hookInvocations++;
          expect(engine.pendingCommitSeq()).toBe(delta.commitSeq);
        });

        try {
          for (const top of steps) {
            if (top.kind === 'atomic') {
              applyAtomicStep(engine, oracle, top.step);
              assertMatchesOracle(engine, oracle);
            } else {
              const savedOuter = cloneOracle(oracle);
              const hooksBefore = hookInvocations;
              let threw = false;
              try {
                engine.transact(
                  () => {
                    const workOuter = cloneOracle(savedOuter);
                    for (const s of top.outerBefore) {
                      applyAtomicStep(engine, workOuter, s);
                    }
                    // Verify RYOW inside outer transact before inner savepoint
                    assertMatchesOracle(engine, workOuter);

                    const savedInner = cloneOracle(workOuter);
                    let innerThrew = false;
                    try {
                      engine.transact(() => {
                        const workInner = cloneOracle(savedInner);
                        for (const s of top.inner) {
                          applyAtomicStep(engine, workInner, s);
                        }
                        assertMatchesOracle(engine, workInner);
                        if (top.abortInner) {
                          throw new Error('intentional inner savepoint abort');
                        }
                        workOuter.entities = workInner.entities;
                        workOuter.edges = workInner.edges;
                        workOuter.counters = workInner.counters;
                      });
                    } catch {
                      innerThrew = true;
                    }
                    expect(innerThrew).toBe(top.abortInner);
                    // After inner savepoint: RYOW must match either committed inner or rolled-back savedInner!
                    assertMatchesOracle(engine, workOuter);

                    for (const s of top.outerAfter) {
                      applyAtomicStep(engine, workOuter, s);
                    }
                    assertMatchesOracle(engine, workOuter);

                    if (top.abortOuter) {
                      throw new Error('intentional outer abort');
                    }
                    oracle = workOuter;
                  },
                  { runBeforeCommit: top.runBeforeCommit },
                );
              } catch {
                threw = true;
              }
              expect(threw).toBe(top.abortOuter);
              if (top.abortOuter) {
                oracle = savedOuter;
                expect(hookInvocations).toBe(hooksBefore);
              } else if (!top.runBeforeCommit) {
                expect(hookInvocations).toBe(hooksBefore);
              }
              assertMatchesOracle(engine, oracle);
            }
          }

          // Primary-to-derived integrity check must be 100% clean after arbitrary nested rollbacks!
          expect(engine.admin.check().clean).toBe(true);
        } finally {
          unsub();
          handle.dispose();
        }
      }),
      { numRuns: 30, seed: 20261126 },
    );
  });
});

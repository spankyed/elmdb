// transact: a unit that throws leaves the engine and its sink as they were; one that returns sends the sink its
// writes together, between onUnitStart and onUnitEnd
import { beforeEach, describe, expect, it } from 'vitest';
import { engine, recordingSink, type Id, type SinkCall } from './helpers.ts';
import type { EngineUnderTest } from './engine-under-test.ts';

let e: EngineUnderTest;
let calls: SinkCall[];

/**
 * Everything a write can change: each entity's attributes, the type index and the relation index. A copy,
 * since a row read from the engine holds the live relation details that `relPatch` changes in place.
 */
function state() {
  const ids = [...e.query.getAllEntities()].sort();
  return structuredClone({
    rows: ids.map((id) => [id, e.query.getAll(id)]),
    types: Object.fromEntries(e.query.getAllEntityTypes().map((t) => [t, [...e.query.getEntitiesOfType(t)].sort()])),
    relations: Object.keys(e.admin.relationIndex).sort().map((kind) => {
      const entry = e.admin.relationIndex[kind];
      const lists = (side: Record<string, Id[]>) => Object.keys(side).sort().map((id) => [id, [...side[id]]]);
      return [kind, lists(entry.bySource), lists(entry.byTarget)];
    }),
  });
}

let task: Id, other: Id, project: Id, rel: Id;

beforeEach(() => {
  const recording = recordingSink();
  e = engine(recording.sink);
  task = e.query.tx('Task').put('title', 'a').add('tags', 'x').put('meta', { n: 1 }).grant('owner').id();
  other = e.query.tx('Task').put('title', 'b').id();
  project = e.query.tx('Project').put('name', 'p').id();
  e.query.tx(project).link('contains', task, { pos: 1 });
  rel = e.query.findRelations({ sourceEntity: project, relationType: 'contains' })[0].id;
  calls = recording.calls;
  calls.length = 0;
});

/** Each write the engine has, made inside a unit that then throws */
const writes: Record<string, () => void> = {
  'create': () => e.query.tx('Task').put('title', 'new'),
  'create with an id': () => e.query.tx('Task-given' as Id, true).put('title', 'new'),
  'put': () => e.query.tx(task).put('title', 'changed'),
  'put on a new attribute': () => e.query.tx(task).put('fresh', 1),
  'add': () => e.query.tx(task).add('tags', 'y'),
  'merge': () => e.query.tx(task).merge('meta', { m: 2 }),
  'drop': () => e.query.tx(task).drop('tags'),
  'dropIf': () => e.query.tx(task).dropIf('tags', 'x'),
  'grant': () => e.query.tx(other).grant('owner'),
  'revoke': () => e.query.tx(task).revoke('owner'),
  'ensure': () => e.query.tx(other).ensure('owner'),
  'link': () => e.query.tx(other).link('blocks', task),
  'linkOne': () => e.query.tx(project).linkOne('contains', task, { pos: 2 }),
  'safeLink symmetric': () => e.query.tx(other).safeLink('peer', task, { symmetric: true }),
  'relPatch': () => e.query.tx(project).relPatch(rel, { targetEntity: other, info: { pos: 9 } }),
  'patchLink': () => e.query.tx(project).patchLink('contains', task, { newTarget: other }),
  'unlink': () => e.query.tx(project).unlink(rel),
  'unlinkIf': () => e.query.tx(project).unlinkIf('contains'),
  'destroy': () => e.query.tx(task).destroy(),
  'destroy without persistence': () => e.query.tx(task).destroy(true),
  'destroy then create the same id': () => { e.query.tx(task).destroy(); e.query.tx(task, true).put('title', 'again'); },
};

describe('a unit that throws', () => {
  it.each(Object.keys(writes))('undoes %s and sends the sink nothing', (name) => {
    const before = state();
    expect(() => e.query.transact(() => { writes[name](); throw new Error('stop'); })).toThrow('stop');
    expect(state()).toEqual(before);
    expect(calls).toEqual([]);
  });

  it('undoes every write it made, in any order, and rethrows', () => {
    const before = state();
    const error = new Error('stop');
    expect(() => e.query.transact(() => {
      for (const write of Object.values(writes).filter((w) => w !== writes['destroy then create the same id'])) {
        try { write(); } catch { /* a write that no longer applies (its entity gone) is skipped */ }
      }
      throw error;
    })).toThrow(error);
    expect(state()).toEqual(before);
    expect(calls).toEqual([]);
  });

  it('leaves a query kept from before it finding what it found', () => {
    const kept = e.query.qx('Task');
    expect(() => e.query.transact(() => { e.query.tx(task).destroy(); throw new Error('stop'); })).toThrow();
    expect(kept.ids()).toEqual([task, other]);
    expect(e.query.qx('Task').ids().sort()).toEqual([task, other].sort());
  });
});

describe('a unit that returns', () => {
  it('returns what fn returned, and reads inside it see its own writes', () => {
    const made = e.query.transact(() => {
      const id = e.query.tx('Task').put('title', 'inside').id();
      expect(e.query.findById(id)).toMatchObject({ title: 'inside' });
      return id;
    });
    expect(e.query.findById(made)).toMatchObject({ title: 'inside' });
  });

  it('sends the sink its writes after it returns, between onUnitStart and onUnitEnd', () => {
    e.query.transact(() => {
      e.query.tx(task).put('title', 'one');
      e.query.tx(other).put('title', 'two');
      expect(calls).toEqual([]);
    });
    expect(calls.map(([method]) => method)).toEqual(['onUnitStart', 'onPutAttrArray', 'onPutAttrArray', 'onUnitEnd']);
  });

  it('sends nothing, not even the bracket, when it wrote nothing', () => {
    e.query.transact(() => e.query.findById(task));
    expect(calls).toEqual([]);
  });
});

describe('a unit inside another', () => {
  it('commits into the outer one: nothing reaches the sink until the outer returns', () => {
    e.query.transact(() => {
      e.query.transact(() => e.query.tx(task).put('title', 'inner'));
      expect(calls).toEqual([]);
    });
    expect(calls.map(([method]) => method)).toEqual(['onUnitStart', 'onPutAttrArray', 'onUnitEnd']);
  });

  it('that throws undoes only its own writes, and the outer keeps going', () => {
    e.query.transact(() => {
      e.query.tx(task).put('title', 'outer');
      expect(() => e.query.transact(() => { e.query.tx(task).put('title', 'inner'); e.query.tx(other).destroy(); throw new Error('stop'); })).toThrow();
      expect(e.query.findById(task)).toMatchObject({ title: 'outer' });
      expect(e.query.findById(other)).toMatchObject({ title: 'b' });
    });
    expect(calls.map(([method]) => method)).toEqual(['onUnitStart', 'onPutAttrArray', 'onUnitEnd']);
    expect(e.query.findById(task)).toMatchObject({ title: 'outer' });
  });

  it('that commits is undone with the outer when the outer throws', () => {
    const before = state();
    expect(() => e.query.transact(() => {
      e.query.tx(task).put('title', 'outer');
      e.query.transact(() => { e.query.tx(task).put('title', 'inner'); e.query.tx('Task').put('title', 'new'); });
      throw new Error('stop');
    })).toThrow();
    expect(state()).toEqual(before);
    expect(calls).toEqual([]);
  });
});

describe('what transact refuses', () => {
  it('an async function, undoing what it wrote before its first await', () => {
    const before = state();
    expect(() => e.query.transact(async () => { e.query.tx(task).put('title', 'sync part'); })).toThrow(/synchronous/);
    expect(state()).toEqual(before);
    expect(calls).toEqual([]);
  });

  it('emptying the engine inside it', () => {
    expect(() => e.query.transact(() => e.admin.clear())).toThrow(/clear\(\) inside transact/);
    expect(e.query.findById(task)).toBeDefined();
  });
});

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
  const ids = [...e.getAllEntities()].sort();
  return structuredClone({
    rows: ids.map((id) => [id, e.getAll(id)]),
    types: Object.fromEntries(e.getAllEntityTypes().map((t) => [t, [...e.getEntitiesOfType(t)].sort()])),
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
  task = e.create('Task').set('title', 'a').add('tags', 'x').set('meta', { n: 1 }).grant('owner').id();
  other = e.create('Task').set('title', 'b').id();
  project = e.create('Project').set('name', 'p').id();
  e.write(project).link('contains', task, { info: { pos: 1 } });
  rel = e.findRelations({ sourceEntity: project, relationType: 'contains' })[0].id;
  calls = recording.calls;
  calls.length = 0;
});

/** Each write the engine has, made inside a unit that then throws */
const writes: Record<string, () => void> = {
  'create': () => e.create('Task').set('title', 'new'),
  'create with an id': () => e.create('Task', { id: 'Task-given' as Id }).set('title', 'new'),
  'upsert creating': () => e.upsert('Task', 'Task-upserted' as Id).set('title', 'new'),
  'set': () => e.write(task).set('title', 'changed'),
  'set on a new attribute': () => e.write(task).set('fresh', 1),
  'setMany': () => e.write(task).setMany({ title: 'changed', fresh: 1 }),
  'add': () => e.write(task).add('tags', 'y'),
  'merge': () => e.write(task).merge('meta', { m: 2 }),
  'delete': () => e.write(task).delete('tags'),
  'deleteAt': () => e.write(task).deleteAt('tags', 0),
  'deleteValue': () => e.write(task).deleteValue('tags', 'x'),
  'grant': () => e.write(other).grant('owner'),
  'revoke': () => e.write(task).revoke('owner'),
  'grantExclusive': () => e.write(other).grantExclusive('owner', [task]),
  'link': () => e.write(other).link('blocks', task),
  'link with replace': () => e.write(project).link('contains', task, { info: { pos: 2 }, replace: true }),
  'link symmetric': () => e.write(other).link('peer', task, { symmetric: true }),
  'updateRelation': () => e.updateRelation(rel, { targetEntity: other, info: { pos: 9 } }),
  'removeRelationById': () => e.removeRelationById(rel),
  'unlink': () => e.write(project).unlink('contains'),
  'unlinkAll': () => e.write(project).unlinkAll(),
  'updateEntity': () => e.updateEntity(task, { title: 'changed', meta: null }),
  'destroy': () => e.write(task).destroy(),
  'destroy keeping it stored': () => e.write(task).destroy({ keepStored: true }),
  'destroy then create the same id': () => { e.write(task).destroy(); e.create('Task', { id: task }).set('title', 'again'); },
};

describe('a unit that throws', () => {
  it.each(Object.keys(writes))('undoes %s and sends the sink nothing', (name) => {
    const before = state();
    expect(() => e.transact(() => { writes[name](); throw new Error('stop'); })).toThrow('stop');
    expect(state()).toEqual(before);
    expect(calls).toEqual([]);
  });

  it('undoes every write it made, in any order, and rethrows', () => {
    const before = state();
    const error = new Error('stop');
    expect(() => e.transact(() => {
      for (const write of Object.values(writes).filter((w) => w !== writes['destroy then create the same id'])) {
        try { write(); } catch { /* a write that no longer applies (its entity gone) is skipped */ }
      }
      throw error;
    })).toThrow(error);
    expect(state()).toEqual(before);
    expect(calls).toEqual([]);
  });

  it('leaves a query kept from before it finding what it found', () => {
    const kept = e.query('Task');
    expect(() => e.transact(() => { e.write(task).destroy(); throw new Error('stop'); })).toThrow();
    // An undone destroy puts the entity back last in its type's order
    expect(kept.ids().sort()).toEqual([task, other].sort());
    expect(e.query('Task').ids().sort()).toEqual([task, other].sort());
  });
});

describe('a unit that returns', () => {
  it('returns what fn returned, and reads inside it see its own writes', () => {
    const made = e.transact(() => {
      const id = e.create('Task').set('title', 'inside').id();
      expect(e.findById(id)).toMatchObject({ title: 'inside' });
      return id;
    });
    expect(e.findById(made)).toMatchObject({ title: 'inside' });
  });

  it('sends the sink its writes after it returns, between onUnitStart and onUnitEnd', () => {
    e.transact(() => {
      e.write(task).set('title', 'one');
      e.write(other).set('title', 'two');
      expect(calls).toEqual([]);
    });
    expect(calls.map(([method]) => method)).toEqual(['onUnitStart', 'onPutAttr', 'onPutAttr', 'onUnitEnd']);
  });

  it('sends nothing, not even the bracket, when it wrote nothing', () => {
    e.transact(() => e.findById(task));
    expect(calls).toEqual([]);
  });
});

describe('a unit inside another', () => {
  it('commits into the outer one: nothing reaches the sink until the outer returns', () => {
    e.transact(() => {
      e.transact(() => e.write(task).set('title', 'inner'));
      expect(calls).toEqual([]);
    });
    expect(calls.map(([method]) => method)).toEqual(['onUnitStart', 'onPutAttr', 'onUnitEnd']);
  });

  it('that throws undoes only its own writes, and the outer keeps going', () => {
    e.transact(() => {
      e.write(task).set('title', 'outer');
      expect(() => e.transact(() => { e.write(task).set('title', 'inner'); e.write(other).destroy(); throw new Error('stop'); })).toThrow();
      expect(e.findById(task)).toMatchObject({ title: 'outer' });
      expect(e.findById(other)).toMatchObject({ title: 'b' });
    });
    expect(calls.map(([method]) => method)).toEqual(['onUnitStart', 'onPutAttr', 'onUnitEnd']);
    expect(e.findById(task)).toMatchObject({ title: 'outer' });
  });

  it('that throws after growing a list the outer wrote sends the outer\'s list, not the grown one', () => {
    e.transact(() => {
      e.write(task).add('tags', 'a');
      expect(() => e.transact(() => { e.write(task).add('tags', 'b'); throw new Error('stop'); })).toThrow();
    });
    expect(e.getAttr(task, 'tags')).toEqual(['x', 'a']);
    const sent = calls.filter(([method, kind]) => method === 'onPutAttr' && kind === 'tags');
    expect(sent.at(-1)?.[3]).toEqual(['x', 'a']);
  });

  it('that commits is undone with the outer when the outer throws', () => {
    const before = state();
    expect(() => e.transact(() => {
      e.write(task).set('title', 'outer');
      e.transact(() => { e.write(task).set('title', 'inner'); e.create('Task').set('title', 'new'); });
      throw new Error('stop');
    })).toThrow();
    expect(state()).toEqual(before);
    expect(calls).toEqual([]);
  });
});

describe('a link\'s target', () => {
  it('must exist when the link is made outside a unit, and nothing is written when it does not', () => {
    const before = state();
    expect(() => e.write(task).link('refers', 'Task-ghost' as Id)).toThrow(/link\(refers, "Task-ghost"\): no entity has this id/);
    expect(state()).toEqual(before);
    expect(calls).toEqual([]);
  });

  it('may be created later in the same unit: a forward reference', () => {
    let later!: Id;
    e.transact(() => {
      e.write(task).link('refers', 'Task-later' as Id);
      later = e.create('Task', { id: 'Task-later' as Id }).set('title', 'later').id();
    });
    expect(e.findRelations({ sourceEntity: task, relationType: 'refers' }).map((r) => r.targetEntity)).toEqual([later]);
  });

  it('missing when the unit commits rolls the whole unit back, a nested unit\'s links included', () => {
    const before = state();
    expect(() => e.transact(() => {
      e.write(task).set('title', 'changed');
      e.transact(() => { e.write(other).link('refers', 'Task-never' as Id); });
    })).toThrow(/link\(refers, "Task-never"\): no entity has this id/);
    expect(state()).toEqual(before);
    expect(calls).toEqual([]);
  });

  it('missing at commit is checked on this link\'s own edge, not on another entity\'s', () => {
    // A relation to nothing that came with loaded data, from an entity this unit doesn't touch
    e.admin.addRelation(other, 'refers', 'Task-ghost' as Id);
    e.transact(() => {
      e.write(task).link('refers', 'Task-ghost' as Id);
      e.write(task).unlink('refers', 'Task-ghost' as Id);
      e.write(task).set('title', 'committed');
    });
    expect(e.findById(task)).toMatchObject({ title: 'committed' });
  });

  it('destroyed later in the unit takes the link with it, which is no error', () => {
    e.transact(() => {
      const gone = e.create('Task').set('title', 'gone').id();
      e.write(task).link('refers', gone);
      e.write(gone).destroy();
    });
    expect(e.findRelations({ sourceEntity: task, relationType: 'refers' })).toEqual([]);
  });
});

describe('a moved relation\'s ends', () => {
  it('must exist when it is moved outside a unit, and nothing is written when one does not', () => {
    const before = state();
    expect(() => e.updateRelation(rel, { targetEntity: 'Task-ghost' as Id })).toThrow(/no entity has the id "Task-ghost"/);
    expect(state()).toEqual(before);
    expect(calls).toEqual([]);
  });

  it('may be one created later in the same unit', () => {
    let later!: Id;
    e.transact(() => {
      e.updateRelation(rel, { targetEntity: 'Task-later' as Id });
      later = e.create('Task', { id: 'Task-later' as Id }).id();
    });
    expect(e.findRelations({ sourceEntity: project, relationType: 'contains' }).map((r) => r.targetEntity)).toEqual([later]);
  });

  it('missing when the unit commits rolls the unit back', () => {
    const before = state();
    expect(() => e.transact(() => {
      e.write(task).set('title', 'changed');
      e.updateRelation(rel, { targetEntity: 'Task-never' as Id });
    })).toThrow(/no entity has the id "Task-never"/);
    expect(state()).toEqual(before);
    expect(calls).toEqual([]);
  });
});

describe('what transact refuses', () => {
  it('an async function, undoing what it wrote before its first await', () => {
    const before = state();
    expect(() => e.transact(async () => { e.write(task).set('title', 'sync part'); })).toThrow(/synchronous/);
    expect(state()).toEqual(before);
    expect(calls).toEqual([]);
  });

  it('an async function, without leaving its rejection unhandled', async () => {
    const unhandled: unknown[] = [];
    const listener = (reason: unknown) => unhandled.push(reason);
    process.on('unhandledRejection', listener);
    try {
      expect(() => e.transact(async () => { throw new Error('async body'); })).toThrow(/synchronous/);
      await new Promise((resolve) => setTimeout(resolve, 0));
    } finally {
      process.off('unhandledRejection', listener);
    }
    expect(unhandled).toEqual([]);
  });

  it('emptying the engine inside it', () => {
    expect(() => e.transact(() => e.admin.clear())).toThrow(/clear\(\) inside transact/);
    expect(e.findById(task)).toBeDefined();
  });
});

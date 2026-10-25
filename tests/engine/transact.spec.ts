// transact: a unit that throws leaves the engine as it was and commits nothing; one that returns commits its writes
// together, once
import { beforeEach, describe, expect, it } from 'vitest';
import { committing, engine, type Id, type RelId } from './helpers.ts';
import type { EngineUnderTest } from './engine-under-test.ts';

let e: EngineUnderTest;
/** The units the store committed since the spec's own setup */
let committed: () => number;

/**
 * Everything a write can change: each entity's attributes, the type index, and the relation index as every read
 * of it sees it — the relations, the kinds, and each entity's edges out and in. A copy, since a row read from the
 * engine holds the live relation details that `relPatch` changes in place.
 */
function state() {
  const ids = [...e.admin.getAllEntities()].sort();
  return structuredClone({
    rows: ids.map((id) => [id, e.getRow(id)]),
    types: Object.fromEntries(Object.keys(e.getSchemaStats().entities).map((t) => [t, [...e.admin.getEntitiesOfType(t)].sort()])),
    relations: e.findRelations().sort((x, y) => x.id.localeCompare(y.id)),
    edges: Object.keys(e.getSchemaStats().relations).sort().map((kind) => [kind, ids.map((id) => [
      id, e.query.withTrashed.ids(id).outEdges(kind), e.query.withTrashed.ids(id).inEdges(kind),
    ])]),
  });
}

let task: Id, other: Id, project: Id, rel: RelId;

beforeEach(() => {
  const store = committing();
  e = engine({ wrap: store.wrap });
  task = e.create('Task').set('title', 'a').append('tags', 'x').set('meta', { n: 1 }).grant('owner').id();
  other = e.create('Task').set('title', 'b').id();
  project = e.create('Project').set('name', 'p').id();
  e.write(project).link('contains', task, { info: { pos: 1 } });
  rel = e.findRelations({ source: project, kind: 'contains' })[0].id;
  const mark = store.commits();
  committed = () => store.commits() - mark;
});

/** Each write the engine has, made inside a unit that then throws */
const writes: Record<string, () => void> = {
  'create': () => e.create('Task').set('title', 'new'),
  'create with an id': () => e.create('Task', { id: 'Task-given' as Id }).set('title', 'new'),
  'upsert creating': () => e.upsert('Task', { id: 'Task-upserted' as Id }).set('title', 'new'),
  'set': () => e.write(task).set('title', 'changed'),
  'set on a new attribute': () => e.write(task).set('fresh', 1),
  'setMany': () => e.write(task).setMany({ title: 'changed', fresh: 1 }),
  'append': () => e.write(task).append('tags', 'y'),
  'merge': () => e.write(task).merge('meta', { m: 2 }),
  'delete': () => e.write(task).delete('tags'),
  'removeAt': () => e.write(task).removeAt('tags', 0),
  'removeValue': () => e.write(task).removeValue('tags', 'x'),
  'grant': () => e.write(other).grant('owner'),
  'revoke': () => e.write(task).revoke('owner'),
  'grantExclusive': () => e.write(other).grantExclusive('owner', [task]),
  'link': () => e.write(other).link('blocks', task),
  'link with other info': () => e.write(project).link('contains', task, { info: { pos: 2 } }),
  'updateRelation': () => e.updateRelation(rel, { target: other, info: { pos: 9 } }),
  'removeRelation': () => e.removeRelation(rel),
  'unlink': () => e.write(project).unlink('contains'),
  'unlinkAll': () => e.write(project).unlinkAll(),
  'set null and delete': () => e.write(task).set('title', null).delete('meta'),
  'trash': () => e.write(task).trash(),
  'destroy': () => e.write(task).destroy(),
  'destroy then create the same id': () => { e.write(task).destroy(); e.create('Task', { id: task }).set('title', 'again'); },
};

describe('a unit that throws', () => {
  it.each(Object.keys(writes))('undoes %s and commits nothing', (name) => {
    const before = state();
    expect(() => e.transact(() => { writes[name](); throw new Error('stop'); })).toThrow('stop');
    expect(state()).toEqual(before);
    expect(committed()).toBe(0);
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
    expect(committed()).toBe(0);
  });

  it('leaves a query kept from before it finding what it found', () => {
    const kept = e.query('Task');
    expect(() => e.transact(() => { e.write(task).destroy(); throw new Error('stop'); })).toThrow();
    // An undone destroy leaves the entity where it was
    expect(kept.ids().sort()).toEqual([task, other].sort());
    expect(e.query('Task').ids().sort()).toEqual([task, other].sort());
  });
});

describe('a unit that returns', () => {
  it('returns what fn returned, and reads inside it see its own writes', () => {
    const made = e.transact(() => {
      const id = e.create('Task').set('title', 'inside').id();
      expect(e.query.ids(id).pickOne()).toMatchObject({ title: 'inside' });
      return id;
    });
    expect(e.query.ids(made).pickOne()).toMatchObject({ title: 'inside' });
  });

  it('commits its writes together, once, when it returns', () => {
    e.transact(() => {
      e.write(task).set('title', 'one');
      e.write(other).set('title', 'two');
      expect(committed()).toBe(0);
    });
    expect(committed()).toBe(1);
  });

  it('commits nothing when it wrote nothing', () => {
    e.transact(() => e.query.ids(task).pickOne());
    expect(committed()).toBe(0);
  });
});

describe('a unit inside another', () => {
  it('commits into the outer one: nothing commits until the outer returns', () => {
    e.transact(() => {
      e.transact(() => e.write(task).set('title', 'inner'));
      expect(committed()).toBe(0);
    });
    expect(committed()).toBe(1);
  });

  it('that throws undoes only its own writes, and the outer keeps going', () => {
    e.transact(() => {
      e.write(task).set('title', 'outer');
      expect(() => e.transact(() => { e.write(task).set('title', 'inner'); e.write(other).destroy(); throw new Error('stop'); })).toThrow();
      expect(e.query.ids(task).pickOne()).toMatchObject({ title: 'outer' });
      expect(e.query.ids(other).pickOne()).toMatchObject({ title: 'b' });
    });
    expect(committed()).toBe(1);
    expect(e.query.ids(task).pickOne()).toMatchObject({ title: 'outer' });
  });

  it('that throws after growing a list the outer wrote leaves the outer\'s list, not the grown one', () => {
    e.transact(() => {
      e.write(task).append('tags', 'a');
      expect(() => e.transact(() => { e.write(task).append('tags', 'b'); throw new Error('stop'); })).toThrow();
    });
    expect(e.getRow(task)?.tags).toEqual(['x', 'a']);
  });

  it('that commits is undone with the outer when the outer throws', () => {
    const before = state();
    expect(() => e.transact(() => {
      e.write(task).set('title', 'outer');
      e.transact(() => { e.write(task).set('title', 'inner'); e.create('Task').set('title', 'new'); });
      throw new Error('stop');
    })).toThrow();
    expect(state()).toEqual(before);
    expect(committed()).toBe(0);
  });
});

describe('a link\'s target', () => {
  it('must exist when the link is made outside a unit, and nothing is written when it does not', () => {
    const before = state();
    expect(() => e.write(task).link('refers', 'Task-ghost' as Id)).toThrow(/link\(refers, "Task-ghost"\): no entity has this id/);
    expect(state()).toEqual(before);
    expect(committed()).toBe(0);
  });

  it('may be created later in the same unit: a forward reference', () => {
    let later!: Id;
    e.transact(() => {
      e.write(task).link('refers', 'Task-later' as Id);
      later = e.create('Task', { id: 'Task-later' as Id }).set('title', 'later').id();
    });
    expect(e.findRelations({ source: task, kind: 'refers' }).map((r) => r.target)).toEqual([later]);
  });

  it('missing when the unit commits rolls the whole unit back, a nested unit\'s links included', () => {
    const before = state();
    expect(() => e.transact(() => {
      e.write(task).set('title', 'changed');
      e.transact(() => { e.write(other).link('refers', 'Task-never' as Id); });
    })).toThrow(/link\(refers, "Task-never"\): no entity has this id/);
    expect(state()).toEqual(before);
    expect(committed()).toBe(0);
  });

  it('missing at commit is checked on this link\'s own edge, not on another entity\'s', () => {
    // A relation to nothing that came with loaded data, from an entity this unit doesn't touch
    e.admin.addRelation(other, 'refers', 'Task-ghost' as Id);
    e.transact(() => {
      e.write(task).link('refers', 'Task-ghost' as Id);
      e.write(task).unlink('refers', 'Task-ghost' as Id);
      e.write(task).set('title', 'committed');
    });
    expect(e.query.ids(task).pickOne()).toMatchObject({ title: 'committed' });
  });

  it('destroyed later in the unit takes the link with it, which is no error', () => {
    e.transact(() => {
      const gone = e.create('Task').set('title', 'gone').id();
      e.write(task).link('refers', gone);
      e.write(gone).destroy();
    });
    expect(e.findRelations({ source: task, kind: 'refers' })).toEqual([]);
  });
});

describe('a moved relation\'s ends', () => {
  it('must exist when it is moved outside a unit, and nothing is written when one does not', () => {
    const before = state();
    expect(() => e.updateRelation(rel, { target: 'Task-ghost' as Id })).toThrow(/no entity has the id "Task-ghost"/);
    expect(state()).toEqual(before);
    expect(committed()).toBe(0);
  });

  it('may be one created later in the same unit', () => {
    let later!: Id;
    e.transact(() => {
      e.updateRelation(rel, { target: 'Task-later' as Id });
      later = e.create('Task', { id: 'Task-later' as Id }).id();
    });
    expect(e.findRelations({ source: project, kind: 'contains' }).map((r) => r.target)).toEqual([later]);
  });

  it('missing when the unit commits rolls the unit back', () => {
    const before = state();
    expect(() => e.transact(() => {
      e.write(task).set('title', 'changed');
      e.updateRelation(rel, { target: 'Task-never' as Id });
    })).toThrow(/no entity has the id "Task-never"/);
    expect(state()).toEqual(before);
    expect(committed()).toBe(0);
  });
});

describe('what transact refuses', () => {
  it('an async function, undoing what it wrote before its first await', () => {
    const before = state();
    expect(() => e.transact(async () => { e.write(task).set('title', 'sync part'); })).toThrow(/synchronous/);
    expect(state()).toEqual(before);
    expect(committed()).toBe(0);
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
    expect(e.query.ids(task).pickOne()).toBeDefined();
  });
});

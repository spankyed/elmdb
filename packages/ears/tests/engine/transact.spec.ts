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

  it('that throws, having held every write the outer made, leaves the outer nothing to commit', () => {
    const sequence = e.lastCommit();
    e.transact(() => {
      expect(() => e.transact(() => { e.write(task).set('title', 'inner'); throw new Error('stop'); })).toThrow();
      expect(() => e.transact(() => {
        e.transact(() => e.write(other).set('title', 'innermost'));
        throw new Error('stop');
      })).toThrow();
    });
    expect(committed()).toBe(0);
    expect(e.lastCommit()).toBe(sequence);
    expect([e.getAttr(task, 'title'), e.getAttr(other, 'title')]).toEqual(['a', 'b']);
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

  it('must exist when the link is made inside a unit too: one written later in the unit is not there yet', () => {
    const before = state();
    expect(() => e.transact(() => {
      e.write(task).link('refers', 'Task-later' as Id);
      e.create('Task', { id: 'Task-later' as Id }).set('title', 'later');
    })).toThrow(/link\(refers, "Task-later"\): no entity has this id/);
    expect(state()).toEqual(before);
    expect(committed()).toBe(0);
  });

  it('written first in the unit is there to link to', () => {
    let later!: Id;
    e.transact(() => {
      later = e.create('Task', { id: 'Task-later' as Id }).set('title', 'later').id();
      e.write(task).link('refers', later);
    });
    expect(e.findRelations({ source: task, kind: 'refers' }).map((r) => r.target)).toEqual([later]);
  });

  it('missing in a unit inside another fails that unit alone, where the link is written, and the outer commits its own', () => {
    e.transact(() => {
      e.write(task).set('title', 'changed');
      expect(() => e.transact(() => {
        e.write(other).set('title', 'inner');
        e.write(other).link('refers', 'Task-never' as Id);
      })).toThrow(/link\(refers, "Task-never"\): no entity has this id/);
      expect(e.getAttr(other, 'title')).toBe('b');
    });
    expect(committed()).toBe(1);
    expect([e.getAttr(task, 'title'), e.getAttr(other, 'title')]).toEqual(['changed', 'b']);
    expect(e.findRelations({ source: other, kind: 'refers' })).toEqual([]);
  });

  it('is asked of the link being written, not of a relation to nothing that loaded data brought', () => {
    // A relation to nothing that came with loaded data, from an entity this unit doesn't touch
    e.admin.addRelation(other, 'refers', 'Task-ghost' as Id);
    e.transact(() => {
      e.write(task).link('refers', other);
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

  it('must exist when it is moved inside a unit too, and the unit goes on with the relation where it was', () => {
    e.transact(() => {
      e.write(task).set('title', 'changed');
      expect(() => e.updateRelation(rel, { target: 'Task-later' as Id })).toThrow(/no entity has the id "Task-later"/);
      const later = e.create('Task', { id: 'Task-later' as Id }).id();
      e.updateRelation(rel, { target: later });
    });
    expect(committed()).toBe(1);
    expect(e.findRelations({ source: project, kind: 'contains' }).map((r) => r.target)).toEqual(['Task-later']);
    expect(e.getAttr(task, 'title')).toBe('changed');
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

  it('an admin call that commits on its own, inside it, and the unit rolls back what it wrote', () => {
    const calls: Array<[string, () => unknown]> = [
      ['putAttr', () => e.admin.putAttr(task, 'title', 'loaded')],
      ['bulkLoadAttr', () => e.admin.bulkLoadAttr(task, 'title', 'loaded')],
      ['deleteAttr', () => e.admin.deleteAttr(task, 'title')],
      ['addRelation', () => e.admin.addRelation(task, 'refers', other)],
      ['bulkLoad', () => e.admin.bulkLoad(() => undefined)],
    ];
    for (const [name, call] of calls) {
      expect(() => e.transact(() => { e.write(task).set('title', 'in the unit'); call(); }), name).toThrow(/inside transact/);
      expect(e.getAttr(task, 'title'), name).toBe('a');
    }
    expect(e.findRelations({ source: task, target: other })).toEqual([]);
  });
});

describe('beforeCommit hook and transactWithoutHook', () => {
  it('does not invoke beforeCommit on empty units or units whose writes all rolled back', () => {
    let runs = 0;
    e.admin.beforeCommit({ wants: () => true, run: () => { runs++; } });

    e.transact(() => {});
    e.transact(() => { e.admin.nextSequence(); });
    e.transact(() => {
      expect(() => e.transact(() => {
        e.write(task).set('title', 'undone');
        throw new Error('rollback');
      })).toThrow('rollback');
    });
    expect(runs).toBe(0);

    e.transact(() => { e.write(task).set('title', 'kept'); });
    expect(runs).toBe(1);
  });

  it('calls the hook\'s wants and run as its methods, on the hook', () => {
    const hook = {
      types: new Set(['Task']),
      runs: 0,
      wants(type: string) { return this.types.has(type); },
      run() { this.runs++; },
    };
    e.admin.beforeCommit(hook);
    e.write(task).set('title', 'wanted');
    expect(hook.runs).toBe(1);
    e.write(project).set('name', 'not wanted');
    expect(hook.runs).toBe(1);
  });

  it('keeps the hook paused across nested transactWithoutHook calls', () => {
    let runs = 0;
    e.admin.beforeCommit({ wants: () => true, run: () => { runs++; } });

    e.admin.transactWithoutHook(() => {
      e.admin.transactWithoutHook(() => {
        e.write(task).set('title', 'inner');
      });
      e.write(other).set('title', 'outer');
    });
    expect(runs).toBe(0);

    // Once the outermost transactWithoutHook ends, normal writes invoke the hook again
    e.write(task).set('title', 'after');
    expect(runs).toBe(1);
  });

  it('triggers beforeCommit for writes outside a unit touching wanted types via link target, relation ops, and grantExclusive', () => {
    const seen: Array<{ entities: Record<string, unknown>; relations: Record<string, unknown> }> = [];
    e.admin.beforeCommit({
      wants: (type) => type === 'Task',
      run: () => { seen.push(e.admin.pendingChanges() as { entities: Record<string, unknown>; relations: Record<string, unknown> }); },
    });

    const project2 = e.create('Project').set('name', 'p2').id();
    e.write(project).set('name', 'renamed');
    e.write(project).link('refs', project2);
    expect(seen, 'writes touching only Project skip the unit hook').toHaveLength(0);

    // Link where source is Project (unwanted) and target is Task (wanted)
    e.write(project).link('contains', other);
    expect(seen).toHaveLength(1);
    expect(Object.keys(seen.at(-1)!.relations)).toHaveLength(1);

    // updateRelation (even changing only info)
    e.updateRelation(rel, { info: { pos: 2 } });
    expect(seen).toHaveLength(2);
    expect(seen.at(-1)!.relations).toHaveProperty(rel);

    // removeRelation
    e.removeRelation(rel);
    expect(seen).toHaveLength(3);
    expect(seen.at(-1)!.relations).toHaveProperty(rel);

    // unlink with target
    e.write(project).unlink('contains', other);
    expect(seen).toHaveLength(4);
    expect(Object.keys(seen.at(-1)!.relations)).toHaveLength(1);

    // unlink without target (could unlink a Task)
    e.write(project).link('contains', task);
    seen.length = 0;
    e.write(project).unlink('contains');
    expect(seen).toHaveLength(1);
    expect(Object.keys(seen.at(-1)!.relations)).toHaveLength(1);

    // grantExclusive revoking role from Task in `among`
    e.write(project).grantExclusive('owner', [task]);
    expect(seen).toHaveLength(2);
    expect(seen.at(-1)!.entities).toHaveProperty(task);
  });
});

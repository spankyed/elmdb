// The engine's contract: query builder starts, filters, ordering and terminals
import { beforeEach, describe, expect, it } from 'vitest';
import { EARS } from '../../src/index.ts';
import { engine, isEntityType, isRelationKind, isRole, type Id, type RelId } from './helpers.ts';
import { freshEngine, type EngineUnderTest } from './engine-under-test.ts';

let e: EngineUnderTest;
let t: Record<'a' | 'b' | 'c' | 'd', Id>;
let p: Id;

beforeEach(() => {
  e = engine();
  const task = (fields: Record<string, unknown>) => e.create('Task').setMany(fields).id();
  t = {
    a: task({ title: 'banana', rank: 2, status: 'open' }),
    b: task({ title: 'apple', rank: 10, status: 'done' }),
    c: task({ title: 'cherry', status: 'open' }),
    d: task({ title: 'apple', rank: 1, status: 'open' }),
  };
  p = e.create('Project').set('title', 'P').id();
  e.write(p).link('contains', t.a).link('contains', t.b).link('owns', t.c);
  e.write(t.a).grant('focus');
});

describe('starts', () => {
  it('a type, types, ids, and every entity', () => {
    expect(e.query('Task').ids()).toEqual([t.a, t.b, t.c, t.d]);
    expect(e.query(['Project', 'Task']).ids()).toEqual([p, t.a, t.b, t.c, t.d]);
    expect(e.query.ids(t.b).ids()).toEqual([t.b]);
    expect(e.query.ids([t.c, 'Task-none' as Id, t.a]).ids()).toEqual([t.c, t.a]);
    expect(e.query.ids('Task-none' as Id).ids()).toEqual([]);
    expect(e.query.all().ids()).toEqual([t.a, t.b, t.c, t.d, p]);
    expect(e.findRelations()).toHaveLength(3);
  });

  it('a builder kept across writes answers as one made after them, whatever its steps', () => {
    const held = {
      indexed: e.query('Task').where('status', 'open'),
      role: e.query('Task').withRole('focus'),
      filtered: e.query('Task').has('rank'),
      ordered: e.query('Task').orderBy('title'),
      everything: e.query.all(),
    };
    // A terminal run before the write doesn't fix what the builder answers after it
    expect(held.indexed.ids()).toEqual([t.a, t.c, t.d]);
    const late = e.create('Task').setMany({ title: 'aardvark', rank: 3, status: 'open' }).grant('focus').id();
    expect(held.indexed.ids()).toEqual([t.a, t.c, t.d, late]);
    expect(held.role.ids()).toEqual([t.a, late]);
    expect(held.filtered.count()).toBe(4);
    expect(held.ordered.first()).toBe(late);
    expect(held.everything.ids()).toContain(late);
  });

  /**
   * A type and an id are claims made to different functions, so each can be held to its own: a name that isn't a
   * registered type is a mistake to report, where one start taking both had to read it as an id that matched
   * nothing, and `undefined` (a namespace lookup that missed) used to start from the whole database.
   */
  it('refuses a name that is not a registered type, undefined and a list holding one included', () => {
    for (const start of ['Memo', undefined, 'Task-1', ['Task', 'Memo']]) {
      expect(() => e.query(start as never)).toThrow(expect.objectContaining({ code: 'UNKNOWN_ENTITY_TYPE' }));
    }
    expect(() => e.query(['Task', 'Memo'] as never)).toThrow('query("Memo"): not a registered entity type');
  });

  it('an id or ids start finds only entities still in the engine', () => {
    e.write(t.b).destroy();
    expect(e.query.ids(t.b).ids()).toEqual([]);
    expect(e.query.ids([t.a, t.b, t.c]).ids()).toEqual([t.a, t.c]);
    // An id without a type prefix, and a type's id that was never created
    expect(e.query.ids('nodash' as Id).ids()).toEqual([]);
    expect(e.query.ids(['Project-none' as Id, p]).ids()).toEqual([p]);
  });

  it('a builder kept across writes drops, at its next step, the ids destroyed since', () => {
    const tasks = e.query('Task');
    const named = e.query.ids([t.a, t.b]);
    e.write(t.b).destroy();
    expect(named.limit(10).ids()).toEqual([t.a]);
    expect(named.has('title').count()).toBe(1);
    expect(tasks.limit(10).ids()).toEqual([t.a, t.c, t.d]);
    expect(tasks.ofType('Task').count()).toBe(3);
    expect(tasks.orderBy('title').ids()).toEqual([t.d, t.a, t.c]);
  });
});

describe('filters', () => {
  it('ofType, within, where, has, lacks, withRole, linkedTo, linkedFrom and linkedWith', () => {
    expect(e.query.all().ofType('Project').ids()).toEqual([p]);
    expect(e.query('Task').within([t.d, t.a]).ids()).toEqual([t.a, t.d]);
    expect(e.query('Task').where('status', 'open').ids()).toEqual([t.a, t.c, t.d]);
    expect(e.query('Task').has('rank').ids()).toEqual([t.a, t.b, t.d]);
    expect(e.query('Task').lacks('rank').ids()).toEqual([t.c]);
    expect(e.query('Task').where('rank', 10).ids()).toEqual([t.b]);
    expect(e.query('Task').where('rank', (rank) => (rank as number) < 3).ids()).toEqual([t.a, t.d]);
    expect(() => e.query('Task').where('rank', undefined as never)).toThrow(expect.objectContaining({ code: 'NO_VALUE' }));
    // A value among several of the attribute's, within what the query already holds
    e.write(t.c).append('tag', 'x').append('tag', 'y');
    e.write(t.d).append('tag', 'y');
    e.write(t.a).set('tag', { k: 'y' });
    expect(e.query('Task').where('tag', 'y').ids()).toEqual([t.c, t.d]);
    expect(e.query.ids([t.d, t.c]).where('tag', 'y').ids()).toEqual([t.d, t.c]);
    // An object is matched by a predicate, never by ===
    expect(e.query('Task').where('tag', { k: 'y' }).ids()).toEqual([]);
    expect(e.query('Task').where('tag', (tag) => (tag as { k?: string }).k === 'y').ids()).toEqual([t.a]);
    expect(e.query('Project').where('tag', 'y').ids()).toEqual([]);
    expect(e.query.all().withRole('focus').ids()).toEqual([t.a]);
    expect(e.query('Task').linkedFrom('contains', p).ids()).toEqual([t.a, t.b]);
    expect(e.query('Project').linkedTo('contains', t.a).ids()).toEqual([p]);
    expect(e.query('Task').linkedWith(p).ids()).toEqual([t.a, t.b, t.c]);
  });

  it('out and in walk edges, by kinds and target types, and outEdges and inEdges return their relation rows', () => {
    expect(e.query.ids(p).out(['contains', 'owns']).ids()).toEqual([t.a, t.b, t.c]);
    expect(e.query.ids(p).out('contains', 'Project').ids()).toEqual([]);
    expect(e.query.ids(p).out('contains', ['Task']).ids()).toEqual([t.a, t.b]);
    expect(e.query.ids([t.a, t.b]).in('contains').ids()).toEqual([p]);
    const edge = (kind: string, source: Id, target: Id) => ({ id: expect.stringMatching(/^Relation-/), kind, source, target });
    expect(e.query.ids(p).outEdges(['owns', 'contains'])).toEqual([edge('owns', p, t.c), edge('contains', p, t.a), edge('contains', p, t.b)]);
    // An edge is findRelations' row
    expect(e.query.ids(p).outEdges('owns')).toEqual(e.findRelations({ source: p, kind: 'owns' }));
    expect(e.query.ids(p).outEdges('contains', 'Project')).toEqual([]);
    // A relation whose target isn't in the engine — as data loaded from files can hold, since link() refuses to make
    // one — is stepped around by outEdges as by out: neither reaches an entity a query couldn't start from
    e.admin.bulkLoadRelation('Relation-ghost' as RelId, 'refers', p, 'Task-ghost' as Id);
    e.write(p).link('refers', t.d);
    expect(e.query.ids(p).outEdges('refers')).toEqual([edge('refers', p, t.d)]);
    expect(e.query.ids(p).out('refers').ids()).toEqual([t.d]);
    expect(e.query.ids(p).out('contains').pick(['title'])).toEqual([{ id: t.a, title: 'banana' }, { id: t.b, title: 'apple' }]);
    expect(e.query.ids(p).out(['contains', 'owns'], 'Task').pick(['rank'])).toEqual([
      // A field the row lacks is left out, as getRow leaves it
      { id: t.a, rank: 2 }, { id: t.b, rank: 10 }, { id: t.c },
    ]);
    // Walking in is the same walk the other way
    expect(e.query.ids(t.a).in('contains').ids()).toEqual([p]);
    expect(e.query.ids(t.a).inEdges('contains')).toEqual([edge('contains', p, t.a)]);
  });
});

describe('ordering', () => {
  it('orderBy numbers and strings, missing values last, reverse and limit', () => {
    expect(e.query('Task').orderBy('rank').ids()).toEqual([t.d, t.a, t.b, t.c]);
    expect(e.query('Task').orderBy('rank', 'desc').ids()).toEqual([t.c, t.b, t.a, t.d]);
    expect(e.query('Task').orderBy('title').ids()).toEqual([t.b, t.d, t.a, t.c]);
    expect(e.query('Task').reverse().ids()).toEqual([t.d, t.c, t.b, t.a]);
    expect(e.query('Task').limit(2).ids()).toEqual([t.a, t.b]);
  });

  it('orderBy sorts by type, then value: a list or object after strings, a missing value or null last, a date by its time', () => {
    const at = (fields: Record<string, unknown>) => e.create('Person').setMany(fields).id();
    const v = {
      text: at({ value: 'pear' }),
      none: at({}),
      nil: at({ value: null }),
      ten: at({ value: 10 }),
      later: at({ value: new Date('2026-02-01') }),
      two: at({ value: 2 }),
      earlier: at({ value: new Date('2026-01-01') }),
      flag: at({ value: true }),
      word: at({ value: 'apple' }),
      list: at({ value: ['x'] }),
    };
    expect(e.query('Person').orderBy('value').ids())
      .toEqual([v.flag, v.two, v.ten, v.earlier, v.later, v.word, v.text, v.list, v.none, v.nil]);
    expect(e.query('Person').orderBy('value', 'desc').ids())
      .toEqual([v.none, v.nil, v.list, v.text, v.word, v.later, v.earlier, v.ten, v.two, v.flag]);
  });

  it('distinct and groupBy', () => {
    expect(e.query.ids([t.a, t.a, t.b]).distinct().ids()).toEqual([t.a, t.b]);
    expect(e.query('Task').distinct('title').ids()).toEqual([t.a, t.b, t.c]);
    const groups = e.query('Task').groupBy('status');
    expect([...groups.keys()]).toEqual(['open', 'done']);
    expect(groups.get('open')).toEqual([t.a, t.c, t.d]);
    expect(groups.get('done')).toEqual([t.b]);
  });
});

describe('terminals', () => {
  it('count, first, last, exists, map, forEach, reduce', () => {
    const q = e.query('Task').where('status', 'open');
    expect([q.count(), q.first(), q.last(), q.exists()]).toEqual([3, t.a, t.d, true]);
    const none = e.query('Task').where('status', 'gone');
    expect([none.count(), none.first(), none.last(), none.exists()]).toEqual([0, undefined, undefined, false]);
    expect(q.map((id) => id.length > 0)).toEqual([true, true, true]);
    const seen: Id[] = [];
    expect(q.forEach((id) => seen.push(id)).count()).toBe(3);
    expect(seen).toEqual([t.a, t.c, t.d]);
    expect(q.reduce((n, id) => n + (id === t.c ? 1 : 0), 0)).toBe(1);
    const ids = q.ids();
    ids.pop();
    expect(q.count()).toBe(3);
  });

  it('first, exists, pickOne and limit read only as far as their answer', () => {
    let read = 0;
    const counted = () => e.query('Task').where('title', () => (read++, true));
    const reads = (run: () => unknown) => {
      read = 0;
      run();
      return read;
    };
    expect(reads(() => counted().first())).toBe(1);
    expect(reads(() => counted().exists())).toBe(1);
    expect(reads(() => counted().pickOne(['title']))).toBe(1);
    expect(reads(() => counted().limit(2).ids())).toBe(2);
    e.write(t.a).link('contains', t.c);
    expect(reads(() => counted().out('contains').first())).toBe(1);
    expect(reads(() => counted().count())).toBe(4);
  });

  it("a predicate that creates an entity of the type it walks doesn't reach it", () => {
    const made: Id[] = [];
    // Capped, so a walk that did reach what it made would end rather than run forever
    const make = () => made.length < 10 && made.push(e.create('Task').set('title', 'new').id());
    const ids = e.query('Task').where('title', () => (make(), true)).ids();
    expect(ids).toEqual([t.a, t.b, t.c, t.d]);
    expect(made).toHaveLength(4);
  });

  it("limit takes the first n, rounded down, and none for a count below one or one that isn't a number", () => {
    expect(e.query('Task').limit(2.5).ids()).toEqual([t.a, t.b]);
    expect(e.query('Task').limit(Number.NaN).ids()).toEqual([]);
    expect(e.query('Task').limit(-1).ids()).toEqual([]);
    expect(e.query('Task').limit(Number.POSITIVE_INFINITY).count()).toBe(4);
  });

  it('pick holds a field named __proto__ as its own, as getRow does', () => {
    e.write(t.a).set('__proto__', { x: 1 });
    const [row] = e.query.ids(t.a).pick(['__proto__']);
    expect(Object.getOwnPropertyDescriptor(row, '__proto__')?.value).toEqual({ x: 1 });
    expect(Object.getPrototypeOf(row)).toBe(Object.prototype);
  });

  it('pick, pickOne and pickAll', () => {
    expect(e.query('Task').where('rank', 1).pick(['id', 'title', 'nope'])).toEqual([{ id: t.d, title: 'apple' }]);
    expect(e.query('Task').pickOne(['title'])).toEqual({ id: t.a, title: 'banana' });
    expect(e.query('Task').where('rank', 99).pickOne(['title'])).toBeUndefined();
    expect(e.query.ids(t.a).pickAll()).toEqual([{ id: t.a, entityType: 'Task', createdAt: expect.any(Number), updatedAt: expect.any(Number), title: 'banana', rank: 2, status: 'open', role: ['focus'] }]);
  });
});

describe('the value index', () => {
  /** What `where(k, v)` must answer, read by walking every entity of the types */
  const walked = (types: string[], k: string, v: unknown) =>
    e.query.withTrashed(types).ids().filter((id) => {
      if (e.query.trashed.ids(id).exists()) return false;
      const value = e.getRow(id)?.[k];
      return value === v || (Array.isArray(value) && value.includes(v));
    });

  it('answers where and withRole as a walk would, through every kind of write once it is built', () => {
    const tags = (id: Id) => e.getRow(id)?.tags as string[] | null;
    // Built by the first read, before any of the writes below
    expect(e.query('Task').where('status', 'open').ids()).toEqual([t.a, t.c, t.d]);
    expect(e.query('Task').where('tags', 'x').ids()).toEqual([]);

    e.write(t.b).set('status', 'open');
    e.write(t.a).set('status', 'done');
    e.write(t.c).set('tags', ['x', 'y']).grant('focus');
    e.write(t.d).append('tags', 'x').append('tags', 'x');
    e.write(t.d).removeValue('tags', 'x');
    expect(tags(t.d)).toEqual(['x']);
    e.write(t.c).removeAt('tags', 0);
    e.write(t.a).revoke('focus');
    const late = e.create('Task').setMany({ status: 'open', tags: ['x'] }).grant('focus').id();
    e.write(t.b).trash();
    expect(() => e.transact(() => {
      e.write(late).set('status', 'done').delete('tags').revoke('focus');
      e.write(t.d).destroy();
      throw new Error('stop');
    })).toThrow('stop');
    e.write(t.c).delete('status');

    for (const [k, v] of [['status', 'open'], ['status', 'done'], ['tags', 'x'], ['tags', 'y']] as const) {
      expect(e.query('Task').where(k, v).ids(), `${k} = ${v}`).toEqual(walked(['Task'], k, v));
    }
    // t.d came back from an undone destroy, so it is last in its type's order now
    expect(e.query('Task').where('status', 'open').ids()).toEqual([late, t.d]);
    expect(e.query('Task').withRole('focus').ids()).toEqual([t.c, late]);
    // A step after the start reads the index too, and keeps what the step before it narrowed to
    expect(e.query('Task').where('tags', 'x').withRole('focus').ids()).toEqual([late]);
    expect(e.query.ids([late, t.d]).where('tags', 'x').ids()).toEqual([late, t.d]);
  });

  it('gives its matches in the order the types and their entities were given, and reads trashed ones only with withTrashed', () => {
    const plan = e.create('Project').set('status', 'open').id();
    expect(e.query(['Project', 'Task']).where('status', 'open').ids()).toEqual([plan, t.a, t.c, t.d]);
    expect(e.query(['Task', 'Project']).where('status', 'open').ids()).toEqual([t.a, t.c, t.d, plan]);
    e.write(t.c).trash();
    expect(e.query('Task').where('status', 'open').ids()).toEqual([t.a, t.d]);
    expect(e.query.withTrashed('Task').where('status', 'open').ids()).toEqual([t.a, t.c, t.d]);
    expect(e.query.trashed('Task').where('status', 'open').ids()).toEqual([t.c]);
  });

  it('is rebuilt from what a clear and a bulk load leave', () => {
    expect(e.query('Task').where('status', 'open').ids()).toHaveLength(3);
    e.admin.clear();
    expect(e.query('Task').where('status', 'open').ids()).toEqual([]);
    e.admin.bulkLoadAttr('Task-loaded' as Id, 'status', 'open');
    e.admin.bulkLoadAttr('Task-loaded' as Id, EARS.AttrKind.Role, ['focus']);
    expect(e.query('Task').where('status', 'open').ids()).toEqual(['Task-loaded']);
    expect(e.query('Task').withRole('focus').ids()).toEqual(['Task-loaded']);
  });

  it('leaves to a walk what it cannot answer: a predicate, an object, NaN and the entity type', () => {
    e.write(t.a).set('meta', { k: 1 }).set('score', Number.NaN);
    expect(e.query('Task').where('rank', (r: unknown) => typeof r === 'number' && r > 1).ids()).toEqual([t.a, t.b]);
    expect(e.query('Task').where('meta', { k: 1 }).ids()).toEqual([]);
    expect(e.query('Task').where('score', Number.NaN).ids()).toEqual([]);
    expect(e.query.all().where('entityType', 'Project').ids()).toEqual([p]);
  });
});

describe('declared indexes', () => {
  /** An engine whose Tasks declare an index on `status`, with enough Tasks that a walk of them is reported */
  function indexedTasks() {
    const warnings: string[] = [];
    const engine = freshEngine({
      isEntityType, isRelationKind, isRole,
      rowRules: (type) => (type === 'Task' ? { required: [], indexes: ['status'] } : undefined),
      warn: (message) => warnings.push(message),
    });
    engine.transact(() => {
      for (let i = 0; i < 2_500; i++) engine.create('Task', { status: i % 2 ? 'open' : 'done', title: `Task ${i}`, tags: [`t${i % 3}`] });
    });
    return { engine, warnings };
  }

  it('a where on a declared field reads its index; on any other, it filters the rows and finds the same matches', () => {
    const { engine, warnings } = indexedTasks();
    const open = engine.query('Task').where('status', 'open').ids();
    expect(open).toHaveLength(1_250);
    expect(warnings).toEqual([]);
    // In the order a walk of the type gives, as the filtered walk finds them
    expect(open).toEqual(engine.query('Task').where('status', (s) => s === 'open').ids());
    expect(engine.query('Task').where('title', 'Task 7').ids()).toEqual([engine.query('Task').ids()[7]]);
    expect(engine.query('Task').where('tags', 't1').count()).toBe(833);
    expect(warnings).toHaveLength(2);
    expect(warnings[0]).toMatch(/^where\('title'\) filtered 2500 Task rows without an index \(.*queries\.spec\.ts.*\)\. Declare it in the Task entry of apack\.json: "indexes": \["title"\]$/);
  });

  it('warns once per call site, and not for a type small enough to walk', () => {
    const { engine, warnings } = indexedTasks();
    // A first() that finds its row early walks too little to report
    engine.query('Task').where('title', 'Task 0').first();
    expect(warnings).toEqual([]);
    for (let i = 0; i < 3; i++) engine.query('Task').where('title', 'Task 2499').first();
    expect(warnings).toHaveLength(1);
    engine.create('Project', { title: 'P' });
    engine.query('Project').where('title', 'P').ids();
    expect(warnings).toHaveLength(1);
  });

  it('keeps a declared index through writes, trash and a unit that throws', () => {
    const { engine } = indexedTasks();
    const [first] = engine.query('Task').where('status', 'done').ids();
    engine.write(first).set('status', 'open');
    expect(engine.query('Task').where('status', 'done').ids()).not.toContain(first);
    expect(engine.query('Task').where('status', 'open').ids()).toContain(first);
    engine.write(first).trash();
    expect(engine.query('Task').where('status', 'open').ids()).not.toContain(first);
    expect(engine.query.withTrashed('Task').where('status', 'open').ids()).toContain(first);
    expect(() => engine.transact(() => {
      engine.write(first).set('status', 'gone');
      throw new Error('stop');
    })).toThrow('stop');
    expect(engine.query.withTrashed('Task').where('status', 'gone').ids()).toEqual([]);
    expect(engine.query.withTrashed('Task').where('status', 'open').ids()).toContain(first);
  });

  it('finds a value no index holds, a long string or an infinite number, by filtering the rows', () => {
    const { engine } = indexedTasks();
    const long = 'x'.repeat(300);
    const [a, b] = engine.query('Task').limit(2).ids();
    engine.write(a).set('status', long);
    engine.write(b).set('status', Infinity);
    expect(engine.query('Task').where('status', long).ids()).toEqual([a]);
    expect(engine.query('Task').where('status', Infinity).ids()).toEqual([b]);
  });
});

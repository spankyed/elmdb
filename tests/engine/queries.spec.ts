// The engine's contract: query builder starts, filters, ordering and terminals; blueprints; the repository registry
import { beforeEach, describe, expect, it } from 'vitest';
import { bp } from '../../src/index.ts';
import { engine, type Id } from './helpers.ts';
import type { EngineUnderTest } from './engine-under-test.ts';

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
  it('a type, types, ids, and every entity but the relations', () => {
    expect(e.qx('Task').ids()).toEqual([t.a, t.b, t.c, t.d]);
    expect(e.qx(['Project', 'Task']).ids()).toEqual([p, t.a, t.b, t.c, t.d]);
    expect(e.qx.ids(t.b).ids()).toEqual([t.b]);
    expect(e.qx.ids([t.c, 'Task-none' as Id, t.a]).ids()).toEqual([t.c, t.a]);
    expect(e.qx.ids('Task-none' as Id).ids()).toEqual([]);
    expect(e.qx.all().ids()).toEqual([t.a, t.b, t.c, t.d, p]);
    expect(e.getEntitiesOfType('Relation')).toHaveLength(3);
  });

  /**
   * A type and an id are claims made to different functions, so each can be held to its own: a name that isn't a
   * registered type is a mistake to report, where one start taking both had to read it as an id that matched
   * nothing, and `undefined` (a namespace lookup that missed) used to start from the whole database.
   */
  it('refuses a name that is not a registered type, undefined and a list holding one included', () => {
    for (const start of ['Memo', undefined, 'Task-1', ['Task', 'Memo']]) {
      expect(() => e.qx(start as never)).toThrow(expect.objectContaining({ code: 'UNKNOWN_ENTITY_TYPE' }));
    }
    expect(() => e.qx(['Task', 'Memo'] as never)).toThrow('qx("Memo"): not a registered entity type');
  });

  it('an id or ids start finds only entities still in the engine', () => {
    e.write(t.b).destroy();
    expect(e.qx.ids(t.b).ids()).toEqual([]);
    expect(e.qx.ids([t.a, t.b, t.c]).ids()).toEqual([t.a, t.c]);
    // An id without a type prefix, and a type's id that was never created
    expect(e.qx.ids('nodash' as Id).ids()).toEqual([]);
    expect(e.qx.ids(['Project-none' as Id, p]).ids()).toEqual([p]);
  });

  it('a builder kept across writes drops, at its next step, the ids destroyed since', () => {
    const tasks = e.qx('Task');
    e.write(t.b).destroy();
    expect(tasks.limit(10).ids()).toEqual([t.a, t.c, t.d]);
    expect(tasks.ofType('Task').count()).toBe(3);
    expect(tasks.orderBy('title').ids()).toEqual([t.d, t.a, t.c]);
  });
});

describe('filters', () => {
  it('ofType, inIds, where, has, lacks, withRole, related and relatedTo', () => {
    expect(e.qx.all().ofType('Project').ids()).toEqual([p]);
    expect(e.qx('Task').inIds([t.d, t.a]).ids()).toEqual([t.a, t.d]);
    expect(e.qx('Task').where('status', 'open').ids()).toEqual([t.a, t.c, t.d]);
    expect(e.qx('Task').has('rank').ids()).toEqual([t.a, t.b, t.d]);
    expect(e.qx('Task').lacks('rank').ids()).toEqual([t.c]);
    expect(e.qx('Task').where('rank', 10).ids()).toEqual([t.b]);
    expect(e.qx('Task').where('rank', (rank) => (rank as number) < 3).ids()).toEqual([t.a, t.d]);
    expect(() => e.qx('Task').where('rank', undefined as never)).toThrow(expect.objectContaining({ code: 'NO_VALUE' }));
    // A value among several of the attribute's, within what the query already holds
    e.write(t.c).add('tag', 'x').add('tag', 'y');
    e.write(t.d).add('tag', 'y');
    e.write(t.a).set('tag', { k: 'y' });
    expect(e.qx('Task').where('tag', 'y').ids()).toEqual([t.c, t.d]);
    expect(e.qx.ids([t.d, t.c]).where('tag', 'y').ids()).toEqual([t.d, t.c]);
    // An object is matched by a predicate, never by ===
    expect(e.qx('Task').where('tag', { k: 'y' }).ids()).toEqual([]);
    expect(e.qx('Task').where('tag', (tag) => (tag as { k?: string }).k === 'y').ids()).toEqual([t.a]);
    expect(e.qx('Project').where('tag', 'y').ids()).toEqual([]);
    expect(e.qx.all().withRole('focus').ids()).toEqual([t.a]);
    expect(e.qx('Task').related('contains', p, true).ids()).toEqual([t.a, t.b]);
    expect(e.qx('Project').related('contains', t.a).ids()).toEqual([p]);
    expect(e.qx('Task').relatedTo(p).ids()).toEqual([t.a, t.b, t.c]);
  });

  it('linksTo navigates, by kinds and target types, and links, linksPick return the links', () => {
    expect(e.qx.ids(p).linksTo(['contains', 'owns']).ids()).toEqual([t.a, t.b, t.c]);
    expect(e.qx.ids(p).linksTo('contains', 'Project').ids()).toEqual([]);
    expect(e.qx.ids(p).linksTo('contains', ['Task']).ids()).toEqual([t.a, t.b]);
    expect(e.qx.ids([t.a, t.b]).linksTo('contains', undefined, false).ids()).toEqual([p]);
    expect(e.qx.ids(p).links(['owns', 'contains'])).toEqual([
      { relation: 'owns', id: t.c }, { relation: 'contains', id: t.a }, { relation: 'contains', id: t.b },
    ]);
    expect(e.qx.ids(p).links('contains', 'Project')).toEqual([]);
    // A link to an id that isn't in the engine is followed by links, not by linksTo
    e.write(p).link('refers', 'Task-ghost' as Id).link('refers', t.d);
    expect(e.qx.ids(p).links('refers')).toEqual([{ relation: 'refers', id: 'Task-ghost' }, { relation: 'refers', id: t.d }]);
    expect(e.qx.ids(p).linksTo('refers').ids()).toEqual([t.d]);
    expect(e.qx.ids(p).linksPick('contains', ['title'])).toEqual([{ id: t.a, title: 'banana' }, { id: t.b, title: 'apple' }]);
    expect(e.qx.ids(p).linksPick(['contains', 'owns'], ['rank'], 'Task')).toEqual([
      { relation: 'contains', id: t.a, rank: 2 }, { relation: 'contains', id: t.b, rank: 10 }, { relation: 'owns', id: t.c, rank: null },
    ]);
  });
});

describe('ordering', () => {
  it('orderBy numbers and strings, missing values last, reverse and limit', () => {
    expect(e.qx('Task').orderBy('rank').ids()).toEqual([t.d, t.a, t.b, t.c]);
    expect(e.qx('Task').orderBy('rank', 'desc').ids()).toEqual([t.c, t.b, t.a, t.d]);
    expect(e.qx('Task').orderBy('title').ids()).toEqual([t.b, t.d, t.a, t.c]);
    expect(e.qx('Task').reverse().ids()).toEqual([t.d, t.c, t.b, t.a]);
    expect(e.qx('Task').limit(2).ids()).toEqual([t.a, t.b]);
  });

  it('distinct and groupBy', () => {
    expect(e.qx.ids([t.a, t.a, t.b]).distinct().ids()).toEqual([t.a, t.b]);
    expect(e.qx('Task').distinct('title').ids()).toEqual([t.a, t.b, t.c]);
    const groups = e.qx('Task').groupBy('status');
    expect([...groups.keys()]).toEqual(['open', 'done']);
    expect(groups.get('open')!.ids()).toEqual([t.a, t.c, t.d]);
    expect(groups.get('done')!.pick(['title'])).toEqual([{ id: t.b, title: 'apple' }]);
  });
});

describe('terminals', () => {
  it('count, first, last, id, exists, map, forEach, reduce', () => {
    const q = e.qx('Task').where('status', 'open');
    expect([q.count(), q.first(), q.last(), q.id(), q.exists()]).toEqual([3, t.a, t.d, t.a, true]);
    const none = e.qx('Task').where('status', 'gone');
    expect([none.count(), none.first(), none.last(), none.id(), none.exists()]).toEqual([0, null, null, null, false]);
    expect(q.map((id) => id.length > 0)).toEqual([true, true, true]);
    const seen: Id[] = [];
    expect(q.forEach((id) => seen.push(id)).count()).toBe(3);
    expect(seen).toEqual([t.a, t.c, t.d]);
    expect(q.reduce((n, id) => n + (id === t.c ? 1 : 0), 0)).toBe(1);
    const ids = q.ids();
    ids.pop();
    expect(q.count()).toBe(3);
  });

  it('pick, pickOne and pickAll', () => {
    expect(e.qx('Task').where('rank', 1).pick(['id', 'title', 'nope'])).toEqual([{ id: t.d, title: 'apple', nope: null }]);
    expect(e.qx('Task').pickOne(['title'])).toEqual({ id: t.a, title: 'banana' });
    expect(e.qx('Task').where('rank', 99).pickOne(['title'])).toBeNull();
    expect(e.qx.ids(t.a).pickAll()).toEqual([{ id: t.a, entityType: 'Task', createdAt: expect.any(Number), title: 'banana', rank: 2, status: 'open', role: 'focus' }]);
  });
});

describe('blueprints', () => {
  it('spawn creates a blueprint graph, reusing shared nodes unless told not to', () => {
    const shared = bp('Person').attr('name', 'Sam').grant('member').build();
    const plan = bp('Project')
      .attr('title', 'Plan')
      .ensure('current')
      .link('owner', shared)
      .link('lead', shared, { since: 1 })
      .link('about', t.a)
      .build();
    expect(plan).toEqual({
      entity: 'Project', attrs: { title: 'Plan' }, uniqueRoles: ['current'],
      rels: [{ kind: 'owner', target: shared, info: undefined }, { kind: 'lead', target: shared, info: { since: 1 } }, { kind: 'about', target: t.a, info: undefined }],
    });
    const id = e.spawn(plan);
    expect(e.findById(id)).toMatchObject({ title: 'Plan', role: 'current' });
    const [owner] = e.qx.ids(id).linksTo('owner').ids();
    expect(e.qx.ids(id).linksTo('lead').ids()).toEqual([owner]);
    expect(e.findById(owner)).toMatchObject({ name: 'Sam', role: 'member' });
    expect(e.findRelations({ sourceEntity: id, relationType: 'lead' })[0].info).toEqual({ since: 1 });
    expect(e.qx.ids(id).linksTo('about').ids()).toEqual([t.a]);

    const again = e.spawn(plan, { dedupe: false });
    expect(e.qx.all().withRole('current').ids()).toEqual([again]);
    expect(e.qx.ids(again).linksTo(['owner', 'lead']).count()).toBe(2);
    expect(e.getEntitiesOfType('Person')).toHaveLength(3);
  });
});

describe('repository registry', () => {
  it.each(['__proto__', 'constructor', 'toString', 'hasOwnProperty'])('owns the repository named %s without inheriting or leaking its members', name => {
    expect(() => e.repository[name]).toThrow('is not registered');
    const value = { owner: 'candidate' };
    e.registerRepository(name, value);
    expect(e.repository[name]).toBe(value);
    const snapshot = e.admin.repositories();
    expect(Object.hasOwn(snapshot, name)).toBe(true);
    expect(snapshot[name]).toBe(value);
    expect(Object.getPrototypeOf(snapshot)).toBe(Object.prototype);
    expect(() => e.repository.owner).toThrow('is not registered');
    delete snapshot[name];
    expect(e.repository[name]).toBe(value);
    e.unregisterRepository(name);
    expect(Object.hasOwn(e.admin.repositories(), name)).toBe(false);
    expect(() => e.repository[name]).toThrow('is not registered');
    expect(() => e.repository.owner).toThrow('is not registered');
  });

  it('returns what was registered and names what wasn\'t', () => {
    const repo = { list: () => 1 };
    e.registerRepository('contractRepo', repo);
    expect(e.repository.contractRepo).toBe(repo);
    e.registerRepository('contractRepo', { list: () => 2 });
    expect((e.repository.contractRepo as typeof repo).list()).toBe(2);
    expect(() => e.repository.contractMissing).toThrow('[repository] "contractMissing" is not registered: a pack declares its repositories in apack.json');
    expect((e.repository as Record<symbol, unknown>)[Symbol.iterator]).toBeUndefined();
  });
});

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
  const task = (fields: Record<string, unknown>) => e.query.tx('Task').batchPut(fields).id();
  t = {
    a: task({ title: 'banana', rank: 2, status: 'open' }),
    b: task({ title: 'apple', rank: 10, status: 'done' }),
    c: task({ title: 'cherry', status: 'open' }),
    d: task({ title: 'apple', rank: 1, status: 'open' }),
  };
  p = e.query.tx('Project').put('title', 'P').id();
  e.query.tx(p).link('contains', t.a).link('contains', t.b).link('owns', t.c);
  e.query.tx(t.a).grant('focus');
});

describe('starts', () => {
  it('no start, a type, types, an id, ids, and unknown ones', () => {
    expect(e.query.qx().count()).toBe(8);
    expect(e.query.qx('Task').ids()).toEqual([t.a, t.b, t.c, t.d]);
    expect(e.query.qx(['Project', 'Task']).ids()).toEqual([p, t.a, t.b, t.c, t.d]);
    expect(e.query.qx(t.b).ids()).toEqual([t.b]);
    expect(e.query.qx([t.c, 'Task-none' as Id, t.a]).ids()).toEqual([t.c, t.a]);
    expect(e.query.qx('Task-none' as Id).ids()).toEqual([]);
    // Deliberate: a name the engine doesn't know as a type is read as an id, so it finds nothing rather than
    // throwing. A start can't be told apart from a typo or a type whose pack isn't installed — the engine has no
    // way to know which was meant — and the case below pins the other side of it, an id with no type prefix.
    expect(e.query.qx('Memo').ids()).toEqual([]);
    expect(e.query.qx(undefined).count()).toBe(8);
  });

  it('an id or ids start finds only entities still in the engine', () => {
    e.query.tx(t.b).destroy();
    expect(e.query.qx(t.b).ids()).toEqual([]);
    expect(e.query.qx([t.a, t.b, t.c]).ids()).toEqual([t.a, t.c]);
    // An id without a type prefix, and a type's id that was never created
    expect(e.query.qx('nodash' as Id).ids()).toEqual([]);
    expect(e.query.qx(['Project-none' as Id, p]).ids()).toEqual([p]);
  });

  it('a builder kept across writes drops, at its next step, the ids destroyed since', () => {
    const tasks = e.query.qx('Task');
    e.query.tx(t.b).destroy();
    expect(tasks.limit(10).ids()).toEqual([t.a, t.c, t.d]);
    expect(tasks.ofType('Task').count()).toBe(3);
    expect(tasks.orderBy('title').ids()).toEqual([t.d, t.a, t.c]);
  });
});

describe('filters', () => {
  it('ofType, inIds, where with and without a value, withRole, related and relatedTo', () => {
    expect(e.query.qx().ofType('Project').ids()).toEqual([p]);
    expect(e.query.qx('Task').inIds([t.d, t.a]).ids()).toEqual([t.a, t.d]);
    expect(e.query.qx('Task').where('status', 'open').ids()).toEqual([t.a, t.c, t.d]);
    expect(e.query.qx('Task').where('rank').ids()).toEqual([t.a, t.b, t.d]);
    expect(e.query.qx('Task').where('rank', 10).ids()).toEqual([t.b]);
    // A value among several of the attribute's, within what the query already holds
    e.query.tx(t.c).add('tag', 'x').add('tag', 'y');
    e.query.tx(t.d).add('tag', 'y');
    expect(e.query.qx('Task').where('tag', 'y').ids()).toEqual([t.c, t.d]);
    expect(e.query.qx([t.d, t.c]).where('tag', 'y').ids()).toEqual([t.d, t.c]);
    expect(e.query.qx('Project').where('tag', 'y').ids()).toEqual([]);
    expect(e.query.qx().withRole('focus').ids()).toEqual([t.a]);
    expect(e.query.qx('Task').related('contains', p, true).ids()).toEqual([t.a, t.b]);
    expect(e.query.qx('Project').related('contains', t.a).ids()).toEqual([p]);
    expect(e.query.qx('Task').relatedTo(p).ids()).toEqual([t.a, t.b, t.c]);
  });

  it('linksTo navigates, by kinds and target types, and links, linksPick return the links', () => {
    expect(e.query.qx(p).linksTo(['contains', 'owns']).ids()).toEqual([t.a, t.b, t.c]);
    expect(e.query.qx(p).linksTo('contains', 'Project').ids()).toEqual([]);
    expect(e.query.qx(p).linksTo('contains', ['Task']).ids()).toEqual([t.a, t.b]);
    expect(e.query.qx([t.a, t.b]).linksTo('contains', undefined, false).ids()).toEqual([p]);
    expect(e.query.qx(p).links(['owns', 'contains'])).toEqual([
      { relation: 'owns', id: t.c }, { relation: 'contains', id: t.a }, { relation: 'contains', id: t.b },
    ]);
    expect(e.query.qx(p).links('contains', 'Project')).toEqual([]);
    // A link to an id that isn't in the engine is followed by links, not by linksTo
    e.query.tx(p).link('refers', 'Task-ghost' as Id).link('refers', t.d);
    expect(e.query.qx(p).links('refers')).toEqual([{ relation: 'refers', id: 'Task-ghost' }, { relation: 'refers', id: t.d }]);
    expect(e.query.qx(p).linksTo('refers').ids()).toEqual([t.d]);
    expect(e.query.qx(p).linksPick('contains', ['title'])).toEqual([{ id: t.a, title: 'banana' }, { id: t.b, title: 'apple' }]);
    expect(e.query.qx(p).linksPick(['contains', 'owns'], ['rank'], 'Task')).toEqual([
      { relation: 'contains', id: t.a, rank: 2 }, { relation: 'contains', id: t.b, rank: 10 }, { relation: 'owns', id: t.c, rank: null },
    ]);
  });
});

describe('ordering', () => {
  it('orderBy numbers and strings, missing values last, reverse, limit and page', () => {
    expect(e.query.qx('Task').orderBy('rank').ids()).toEqual([t.d, t.a, t.b, t.c]);
    expect(e.query.qx('Task').orderBy('rank', 'desc').ids()).toEqual([t.c, t.b, t.a, t.d]);
    expect(e.query.qx('Task').orderBy('title').ids()).toEqual([t.b, t.d, t.a, t.c]);
    expect(e.query.qx('Task').reverse().ids()).toEqual([t.d, t.c, t.b, t.a]);
    expect(e.query.qx('Task').limit(2).ids()).toEqual([t.a, t.b]);
    const first = e.query.qx('Task').page(3);
    expect(first).toEqual({ items: [t.a, t.b, t.c], nextCursor: btoa('3') });
    expect(e.query.qx('Task').page(3, first.nextCursor)).toEqual({ items: [t.d], nextCursor: null });
    expect(e.query.qx('Task').page(2, 'not base64!')).toEqual({ items: [t.a, t.b], nextCursor: btoa('2') });
  });

  it('distinct and groupBy', () => {
    expect(e.query.qx([t.a, t.a, t.b]).distinct().ids()).toEqual([t.a, t.b]);
    expect(e.query.qx('Task').distinct('title').ids()).toEqual([t.a, t.b, t.c]);
    const groups = e.query.qx('Task').groupBy('status');
    expect([...groups.keys()]).toEqual(['open', 'done']);
    expect(groups.get('open')!.ids()).toEqual([t.a, t.c, t.d]);
    expect(groups.get('done')!.pick(['title'])).toEqual([{ id: t.b, title: 'apple' }]);
  });
});

describe('terminals', () => {
  it('count, first, last, id, exists, map, forEach, reduce', () => {
    const q = e.query.qx('Task').where('status', 'open');
    expect([q.count(), q.first(), q.last(), q.id(), q.exists()]).toEqual([3, t.a, t.d, t.a, true]);
    const none = e.query.qx('Task').where('status', 'gone');
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
    expect(e.query.qx('Task').where('rank', 1).pick(['id', 'title', 'nope'])).toEqual([{ id: t.d, title: 'apple', nope: null }]);
    expect(e.query.qx('Task').pickOne(['title'])).toEqual({ id: t.a, title: 'banana' });
    expect(e.query.qx('Task').where('rank', 99).pickOne(['title'])).toBeNull();
    expect(e.query.qx(t.a).pickAll()).toEqual([{ id: t.a, entityType: 'Task', createdAt: expect.any(Number), title: 'banana', rank: 2, status: 'open', role: 'focus' }]);
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
    const id = e.query.spawn(plan);
    expect(e.query.findById(id)).toMatchObject({ title: 'Plan', role: 'current' });
    const [owner] = e.query.qx(id).linksTo('owner').ids();
    expect(e.query.qx(id).linksTo('lead').ids()).toEqual([owner]);
    expect(e.query.findById(owner)).toMatchObject({ name: 'Sam', role: 'member' });
    expect(e.query.findRelations({ sourceEntity: id, relationType: 'lead' })[0].info).toEqual({ since: 1 });
    expect(e.query.qx(id).linksTo('about').ids()).toEqual([t.a]);

    const again = e.query.spawn(plan, { dedupe: false });
    expect(e.query.qx().withRole('current').ids()).toEqual([again]);
    expect(e.query.qx(again).linksTo(['owner', 'lead']).count()).toBe(2);
    expect(e.query.getEntitiesOfType('Person')).toHaveLength(3);
  });
});

describe('repository registry', () => {
  it('returns what was registered and names what wasn\'t', () => {
    const repo = { list: () => 1 };
    e.query.registerRepository('contractRepo', repo);
    expect(e.query.repository.contractRepo).toBe(repo);
    e.query.registerRepository('contractRepo', { list: () => 2 });
    expect((e.query.repository.contractRepo as typeof repo).list()).toBe(2);
    expect(() => e.query.repository.contractMissing).toThrow('[repository] "contractMissing" is not registered: a pack declares its repositories in apack.json');
    expect((e.query.repository as Record<symbol, unknown>)[Symbol.iterator]).toBeUndefined();
  });
});

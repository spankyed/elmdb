// The engine's contract: transactions create, update and delete entities, with the ids and fields they return
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { engine, type Id } from './helpers.ts';
import type { EngineUnderTest } from './engine-under-test.ts';

const NOW = 1_700_000_000_000;
let e: EngineUnderTest;

beforeEach(() => {
  vi.useFakeTimers({ now: NOW, toFake: ['Date'] });
  e = engine();
});
afterEach(() => vi.useRealTimers());

describe('tx create', () => {
  it('creates an entity of a type with a new id and createdAt', () => {
    const id = e.query.tx('Task').put('title', 'Write specs').id();
    expect(id).toMatch(/^Task-[a-z0-9]+$/);
    expect(e.query.findById(id)).toEqual({ id, entityType: 'Task', createdAt: NOW, title: 'Write specs' });
    expect(e.query.getEntitiesOfType('Task')).toEqual([id]);
    expect(e.query.getAllEntities()).toEqual([id]);
  });

  /**
   * The attribute is a copy of what the id already says, and the two must not disagree: the engine indexes
   * an entity by its id's prefix, while features and the database console filter on the attribute. A row
   * with one and not the other is found by `qx(type)` and skipped by everything reading rows.
   */
  it('stamps entityType to match the id, on both creation paths, without overriding an explicit one', () => {
    const made = e.query.tx('Task').id();
    expect(e.query.getAttr(made, 'entityType')).toBe('Task');

    const given = e.query.tx('Task-custom' as Id, true).id();
    expect(e.query.getAttr(given, 'entityType')).toBe('Task');

    // Written before the caller's puts, so a caller that sets it still decides
    const overridden = e.query.tx('Task').put('entityType', 'Project').id();
    expect(e.query.getAttr(overridden, 'entityType')).toBe('Project');

    // Writing to an existing row is not a creation, so nothing is stamped
    const plain = e.query.tx('Task').id();
    e.query.tx(plain).put('title', 'later');
    expect(e.query.getAttr(plain, 'entityType')).toBe('Task');
  });

  it('gives each entity its own id', () => {
    const ids = [e.query.tx('Task').id(), e.query.tx('Task').id(), e.query.tx('Project').id()];
    expect(new Set(ids).size).toBe(3);
    expect(e.query.getEntitiesOfType('Task')).toEqual(ids.slice(0, 2));
    expect(e.query.getAllEntityTypes()).toEqual(['Task', 'Project']);
  });

  it('uses a provided id, with createdAt, when asked to', () => {
    const id = e.query.tx('Task-custom' as Id, true).put('title', 'Mine').id();
    expect(id).toBe('Task-custom');
    expect(e.query.findById(id)).toEqual({ id, entityType: 'Task', createdAt: NOW, title: 'Mine' });
  });

  it('writes to an existing id without createdAt, and an unknown type name is taken as an id', () => {
    const id = e.query.tx('Task').id();
    vi.setSystemTime(NOW + 5);
    e.query.tx(id).put('title', 'Later');
    expect(e.query.getAttr(id, 'createdAt')).toBe(NOW);
    expect(e.query.tx('Nope' as Id).id()).toBe('Nope');
    expect(e.query.getAllEntities()).toEqual([id]);
  });

  it('batchPut, define and createEntityWithDefaults write several fields', () => {
    const a = e.query.tx('Task').batchPut({ title: 'A', done: false }).id();
    const b = e.query.tx('Task').define({ attributes: { title: 'B' }, links: ['blocks', a], roles: ['first', 'second'] }).id();
    expect(e.query.findById(a)).toEqual({ id: a, entityType: 'Task', createdAt: NOW, title: 'A', done: false });
    expect(e.query.findById(b)).toEqual({ id: b, entityType: 'Task', createdAt: NOW, title: 'B', role: ['first', 'second'] });
    expect(e.query.qx(b).linksTo('blocks').ids()).toEqual([a]);

    const created = e.query.createEntityWithDefaults('Project', { color: 'red' });
    expect(created).toEqual({
      id: expect.stringMatching(/^Project-/), color: 'red', entityType: 'Project',
      shortCode: 'PRO-1', label: 'New Project 1', createdAt: NOW, updatedAt: NOW,
    });
    expect(e.query.findById(created.id)).toEqual({ ...created });
    const second = e.query.createEntityWithDefaults('Project', { label: 'Named' }, 'PX', 'Project-given' as Id);
    expect(second).toMatchObject({ id: 'Project-given', shortCode: 'PX-2', label: 'Named' });
    expect(e.query.generateShortCode('Project', 'P')).toBe('P-3');
    expect(e.query.generateLabelWithCount('Board', 'Project')).toBe('Board 3');
  });
});

describe('tx update', () => {
  it('put replaces, add and put(…, true) append, update replaces', () => {
    const id = e.query.tx('Task').put('tag', 'a').id();
    e.query.tx(id).add('tag', 'b').put('tag', 'c', true);
    expect(e.query.getAttrs(id, 'tag')).toEqual(['a', 'b', 'c']);
    expect(e.query.findById(id)).toMatchObject({ tag: ['a', 'b', 'c'] });
    e.query.tx(id).put('tag', 'z');
    expect(e.query.getAttrs(id, 'tag')).toEqual(['z']);
    e.query.tx(id).update('tag', 'y').updateBatch({ tag: 'x', other: 1 });
    expect(e.query.findById(id)).toEqual({ id, entityType: 'Task', createdAt: NOW, tag: 'x', other: 1 });
  });

  it('merge combines objects at an index and fills gaps with null', () => {
    const id = e.query.tx('Task').put('meta', { a: 1, b: 1 }).id();
    e.query.tx(id).merge('meta', { b: 2, c: 3 }).merge('meta', 'second', 2);
    expect(e.query.getAttrs(id, 'meta')).toEqual([{ a: 1, b: 2, c: 3 }, null, 'second']);
    e.query.tx(id).merge('meta', 'plain', 0);
    expect(e.query.getAttr(id, 'meta')).toBe('plain');
    e.query.tx(id).merge('fresh', 'v', 1);
    expect(e.query.getAttrs(id, 'fresh')).toEqual([null, 'v']);
  });

  it('drop removes by index, dropIf by value or by matching object fields', () => {
    const id = e.query.tx('Task').put('tag', 'a').add('tag', 'b').add('tag', { k: 1, x: 2 }).id();
    e.query.tx(id).drop('tag', 1);
    expect(e.query.getAttrs(id, 'tag')).toEqual(['a', { k: 1, x: 2 }]);
    e.query.tx(id).dropIf('tag', { k: 1 });
    expect(e.query.getAttrs(id, 'tag')).toEqual(['a']);
    e.query.tx(id).dropIf('tag', 'missing').drop('absent');
    e.query.tx(id).drop('tag');
    expect(e.query.getAttrs(id, 'tag')).toEqual([]);
    expect(e.query.getAttr(id, 'tag')).toBeNull();
    expect(e.query.findById(id)).toEqual({ id, entityType: 'Task', createdAt: NOW });
  });

  it('updateEntity writes fields and updatedAt, drops nulls and replaces arrays', () => {
    const id = e.query.tx('Task').put('title', 'A').put('gone', 1).add('list', 1).add('list', 2).id();
    vi.setSystemTime(NOW + 10);
    e.query.updateEntity(id, { title: 'B', gone: null, list: [3], skipped: undefined });
    expect(e.query.findById(id)).toEqual({ id, entityType: 'Task', createdAt: NOW, updatedAt: NOW + 10, title: 'B', list: [3] });
    e.query.updateEntity(id, { title: 'C' }, true);
    expect(e.query.getAttr(id, 'updatedAt')).toBe(NOW + 10);
  });
});

describe('tx delete', () => {
  it('destroy removes the entity, its attributes and its relations', () => {
    const a = e.query.tx('Task').put('title', 'A').id();
    const b = e.query.tx('Task').put('title', 'B').id();
    const c = e.query.tx('Project').id();
    e.query.tx(a).link('blocks', b);
    e.query.tx(c).link('contains', a);
    e.query.tx(c).link('contains', b);
    e.query.tx(a).destroy();
    expect(e.query.findById(a)).toBeUndefined();
    expect(e.query.getAttr(a, 'title')).toBeNull();
    expect(e.query.getEntitiesOfType('Task')).toEqual([b]);
    expect(e.query.findRelations().map((r) => [r.sourceEntity, r.relationType, r.targetEntity])).toEqual([[c, 'contains', b]]);
    expect(e.query.getAllRelationKinds()).toEqual(['contains']);
    expect(e.query.qx(c).linksTo('contains').ids()).toEqual([b]);
  });

  it('destroyEntity forgets an emptied type', () => {
    const a = e.query.tx('Person').id();
    e.query.destroyEntity(a);
    expect(e.query.getAllEntityTypes()).toEqual([]);
    expect(e.query.getAllEntities()).toEqual([]);
  });
});

describe('finders', () => {
  it('skip soft-deleted rows, except the raw ones', () => {
    const a = e.query.tx('Task').batchPut({ title: 'A', status: 'open' }).id();
    const b = e.query.tx('Task').batchPut({ title: 'B', status: 'open', deleted: true }).id();
    const c = e.query.tx('Task').batchPut({ title: 'C', status: 'done' }).id();
    e.query.tx(c).grant('pinned');
    e.query.tx(b).grant('pinned');
    expect(e.query.findById(b)).toBeUndefined();
    expect(e.query.findByIdRaw(b)).toMatchObject({ id: b, deleted: true });
    expect(e.query.findAll<{ id: Id }>('Task').map((r) => r.id)).toEqual([a, c]);
    expect(e.query.findWhere<{ id: Id }>('Task', 'status', 'open').map((r) => r.id)).toEqual([a]);
    expect(e.query.findFirst<{ id: Id }>('Task', 'status', 'done')?.id).toBe(c);
    expect(e.query.findWithFields('Task', ['title'])).toEqual([{ id: a, title: 'A' }, { id: b, title: 'B' }, { id: c, title: 'C' }]);
    expect(e.query.findByIdWithFields(a, ['title', 'missing'])).toEqual({ id: a, title: 'A', missing: null });
    expect(e.query.findWithRole<{ id: Id }>('Task', 'pinned').map((r) => r.id)).toEqual([c]);
    expect(e.query.findFirstWithRole<{ id: Id }>('Task', 'pinned')?.id).toBe(c);
    expect(e.query.countEntities('Task')).toBe(2);
    expect(e.query.exists(b)).toBe(true);
    expect(e.query.exists('Task-none' as Id)).toBe(false);
    expect(e.query.hasIdCollision(a)).toBe(true);
    expect(e.query.hasIdCollision(undefined)).toBe(false);
  });

  it('read every field of a row, attribute kinds and stats', () => {
    const a = e.query.tx('Task').put('title', 'A').add('tag', 1).add('tag', 2).id();
    const p = e.query.tx('Project').id();
    e.query.tx(p).link('contains', a);
    expect(e.query.getAll(a)).toEqual({ entityType: 'Task', createdAt: NOW, title: 'A', tag: [1, 2] });
    expect(e.query.getAllAttributeKinds()).toEqual(['entityType', 'createdAt', 'title', 'tag', 'relationDetails']);
    expect(e.query.getAttributeStats('tag')).toEqual({ entityCount: 1, totalValues: 2 });
    expect(e.query.getSchemaStats()).toEqual({
      entities: { Task: 1, Project: 1, Relation: 1 },
      attributes: { entityType: 2, createdAt: 2, title: 1, tag: 2, relationDetails: 1 },
      relations: { contains: 1 },
    });
    expect(e.query.queryEntitiesByAttribute('tag')).toEqual([a]);
    expect(e.query.queryEntitiesByAttribute('title', 'A')).toEqual([a]);
    expect(e.query.queryEntitiesByAttribute('title', 'Z')).toEqual([]);
  });

  it('isEntityType answers from the engine\'s checker', () => {
    expect(e.query.isEntityType('Task')).toBe(true);
    expect(e.query.isEntityType('Memo')).toBe(false);
  });
});

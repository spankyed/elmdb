// The engine's contract: create, upsert and write create, change and delete entities, each held to the claim it
// makes about its argument, with the ids and fields they return
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

describe('create', () => {
  it('creates an entity of a type with a new id and createdAt', () => {
    const id = e.query.create('Task').set('title', 'Write specs').id();
    expect(id).toMatch(/^Task-[a-z0-9]+$/);
    expect(e.query.findById(id)).toEqual({ id, entityType: 'Task', createdAt: NOW, title: 'Write specs' });
    expect(e.query.getEntitiesOfType('Task')).toEqual([id]);
    expect(e.query.getAllEntities()).toEqual([id]);
  });

  it('gives each entity its own id', () => {
    const ids = [e.query.create('Task').id(), e.query.create('Task').id(), e.query.create('Project').id()];
    expect(new Set(ids).size).toBe(3);
    expect(e.query.getEntitiesOfType('Task')).toEqual(ids.slice(0, 2));
    expect(e.query.getAllEntityTypes()).toEqual(['Task', 'Project']);
  });

  it('uses an id and a creation time it is given', () => {
    const id = e.query.create('Task', { id: 'Task-custom' as Id, createdAt: 42 }).set('title', 'Mine').id();
    expect(id).toBe('Task-custom');
    expect(e.query.findById(id)).toEqual({ id, entityType: 'Task', createdAt: 42, title: 'Mine' });
  });

  it('refuses a name that is not a registered type, an id in use, and an id naming another type', () => {
    expect(() => e.query.create('Memo' as never)).toThrow(expect.objectContaining({ code: 'UNKNOWN_ENTITY_TYPE' }));
    expect(() => e.query.create('Task-1' as never)).toThrow(/looks like an id: use write\(id\)/);
    const id = e.query.create('Task').id();
    expect(() => e.query.create('Task', { id })).toThrow(expect.objectContaining({ code: 'ID_IN_USE' }));
    expect(() => e.query.create('Task', { id: 'Project-1' as Id })).toThrow(expect.objectContaining({ code: 'TYPE_MISMATCH' }));
    expect(e.query.getAllEntities()).toEqual([id]);
  });

  it('createEntityWithDefaults fills in a short code, a label and timestamps', () => {
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

describe('write', () => {
  it('writes to an existing entity, leaving its createdAt', () => {
    const id = e.query.create('Task').id();
    vi.setSystemTime(NOW + 5);
    e.query.write(id).set('title', 'Later');
    expect(e.query.findById(id)).toEqual({ id, entityType: 'Task', createdAt: NOW, title: 'Later' });
  });

  /** An id no entity has used to be written to anyway, leaving attributes with no entity of a type behind them */
  it('refuses an id no entity has, writing nothing', () => {
    expect(() => e.query.write('Task-missing' as Id)).toThrow(expect.objectContaining({ code: 'NO_SUCH_ENTITY' }));
    const gone = e.query.create('Task').id();
    e.query.write(gone).destroy();
    expect(() => e.query.write(gone).set('title', 'back')).toThrow(/no entity has this id/);
    expect(e.query.getAllEntities()).toEqual([]);
  });
});

describe('upsert', () => {
  it('creates the entity with the id when none has it, and writes to it when one does', () => {
    const id = 'Task-singleton' as Id;
    e.query.upsert('Task', id).set('title', 'first');
    vi.setSystemTime(NOW + 5);
    e.query.upsert('Task', id).set('title', 'second');
    expect(e.query.findById(id)).toEqual({ id, entityType: 'Task', createdAt: NOW, title: 'second' });
  });

  it('refuses an unregistered type, and an entity of another type', () => {
    expect(() => e.query.upsert('Memo' as never, 'Memo-1' as Id)).toThrow(expect.objectContaining({ code: 'UNKNOWN_ENTITY_TYPE' }));
    const id = e.query.create('Task').id();
    expect(() => e.query.upsert('Project', id)).toThrow(expect.objectContaining({ code: 'TYPE_MISMATCH' }));
  });
});

describe('attribute writes', () => {
  it('set replaces, add appends, setMany sets each', () => {
    const id = e.query.create('Task').set('tag', 'a').id();
    e.query.write(id).add('tag', 'b').add('tag', 'c');
    expect(e.query.getAttrs(id, 'tag')).toEqual(['a', 'b', 'c']);
    expect(e.query.findById(id)).toMatchObject({ tag: ['a', 'b', 'c'] });
    e.query.write(id).set('tag', 'z');
    expect(e.query.getAttrs(id, 'tag')).toEqual(['z']);
    e.query.write(id).setMany({ tag: 'x', other: 1 });
    expect(e.query.findById(id)).toEqual({ id, entityType: 'Task', createdAt: NOW, tag: 'x', other: 1 });
  });

  it('merge combines objects at an index and fills gaps with null', () => {
    const id = e.query.create('Task').set('meta', { a: 1, b: 1 }).id();
    e.query.write(id).merge('meta', { b: 2, c: 3 }).merge('meta', 'second', 2);
    expect(e.query.getAttrs(id, 'meta')).toEqual([{ a: 1, b: 2, c: 3 }, null, 'second']);
    e.query.write(id).merge('meta', 'plain', 0);
    expect(e.query.getAttr(id, 'meta')).toBe('plain');
    e.query.write(id).merge('fresh', 'v', 1);
    expect(e.query.getAttrs(id, 'fresh')).toEqual([null, 'v']);
  });

  it('deleteAt removes by index, deleteValue by value or by matching object fields, delete every value', () => {
    const id = e.query.create('Task').set('tag', 'a').add('tag', 'b').add('tag', { k: 1, x: 2 }).add('tag', 'c').id();
    e.query.write(id).deleteAt('tag', 1);
    expect(e.query.getAttrs(id, 'tag')).toEqual(['a', { k: 1, x: 2 }, 'c']);
    e.query.write(id).deleteValue('tag', { k: 1 });
    expect(e.query.getAttrs(id, 'tag')).toEqual(['a', 'c']);
    e.query.write(id).deleteValue('tag', 'missing').delete('absent');
    e.query.write(id).delete('tag');
    expect(e.query.getAttrs(id, 'tag')).toEqual([]);
    expect(e.query.getAttr(id, 'tag')).toBeNull();
    expect(e.query.findById(id)).toEqual({ id, entityType: 'Task', createdAt: NOW });
  });

  it.each(['id', 'entityType', 'createdAt', 'role', 'relationDetails'])('refuses %s, which only the engine writes', (name) => {
    const id = e.query.create('Task').id();
    const before = e.query.getAll(id);
    const w = () => e.query.write(id);
    for (const attempt of [
      () => w().set(name, 'x'), () => w().add(name, 'x'), () => w().merge(name, {}), () => w().delete(name),
      () => w().deleteAt(name, 0), () => w().deleteValue(name, 'x'), () => w().setMany({ title: 'kept out', [name]: 'x' }),
    ]) {
      expect(attempt).toThrow(expect.objectContaining({ code: 'RESERVED_ATTRIBUTE' }));
    }
    // setMany refused before writing any field
    expect(e.query.getAll(id)).toEqual(before);
  });

  it('updateEntity writes fields and updatedAt, deletes nulls and replaces arrays', () => {
    const id = e.query.create('Task').set('title', 'A').set('gone', 1).add('list', 1).add('list', 2).id();
    vi.setSystemTime(NOW + 10);
    e.query.updateEntity(id, { title: 'B', gone: null, list: [3], skipped: undefined });
    expect(e.query.findById(id)).toEqual({ id, entityType: 'Task', createdAt: NOW, updatedAt: NOW + 10, title: 'B', list: [3] });
    e.query.updateEntity(id, { title: 'C' }, true);
    expect(e.query.getAttr(id, 'updatedAt')).toBe(NOW + 10);
  });
});

describe('destroy', () => {
  it('removes the entity, its attributes and its relations', () => {
    const a = e.query.create('Task').set('title', 'A').id();
    const b = e.query.create('Task').set('title', 'B').id();
    const c = e.query.create('Project').id();
    e.query.write(a).link('blocks', b);
    e.query.write(c).link('contains', a);
    e.query.write(c).link('contains', b);
    expect(e.query.write(a).destroy()).toBeUndefined();
    expect(e.query.findById(a)).toBeUndefined();
    expect(e.query.getAttr(a, 'title')).toBeNull();
    expect(e.query.getEntitiesOfType('Task')).toEqual([b]);
    expect(e.query.findRelations().map((r) => [r.sourceEntity, r.relationType, r.targetEntity])).toEqual([[c, 'contains', b]]);
    expect(e.query.getAllRelationKinds()).toEqual(['contains']);
    expect(e.query.qx.ids(c).linksTo('contains').ids()).toEqual([b]);
  });

  it('destroyEntity forgets an emptied type', () => {
    const a = e.query.create('Person').id();
    e.query.destroyEntity(a);
    expect(e.query.getAllEntityTypes()).toEqual([]);
    expect(e.query.getAllEntities()).toEqual([]);
  });
});

describe('finders', () => {
  it('skip soft-deleted rows, except the raw ones', () => {
    const a = e.query.create('Task').setMany({ title: 'A', status: 'open' }).id();
    const b = e.query.create('Task').setMany({ title: 'B', status: 'open', deleted: true }).id();
    const c = e.query.create('Task').setMany({ title: 'C', status: 'done' }).id();
    e.query.write(c).grant('pinned');
    e.query.write(b).grant('pinned');
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
    const a = e.query.create('Task').set('title', 'A').add('tag', 1).add('tag', 2).id();
    const p = e.query.create('Project').id();
    e.query.write(p).link('contains', a);
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

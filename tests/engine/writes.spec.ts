// The engine's contract: create, upsert and write create, change and delete entities, each held to the claim it
// makes about its argument, with the ids and fields they return
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { engine, isEntityType, isRelationKind, isRole, recordingSink, type Id } from './helpers.ts';
import { freshEngine, type EngineUnderTest } from './engine-under-test.ts';
import type { WriteBuilder } from '../../src/index.ts';

const NOW = 1_700_000_000_000;
let e: EngineUnderTest;

beforeEach(() => {
  vi.useFakeTimers({ now: NOW, toFake: ['Date'] });
  e = engine();
});
afterEach(() => vi.useRealTimers());

describe('create', () => {
  it('creates an entity of a type with a new id and createdAt', () => {
    const id = e.create('Task').set('title', 'Write specs').id();
    expect(id).toMatch(/^Task-[a-z0-9]+$/);
    expect(e.findById(id)).toEqual({ id, entityType: 'Task', createdAt: NOW, updatedAt: NOW, title: 'Write specs' });
    expect(e.admin.getEntitiesOfType('Task')).toEqual([id]);
    expect(e.admin.getAllEntities()).toEqual([id]);
  });

  it('gives each entity its own id', () => {
    const ids = [e.create('Task').id(), e.create('Task').id(), e.create('Project').id()];
    expect(new Set(ids).size).toBe(3);
    expect(e.admin.getEntitiesOfType('Task')).toEqual(ids.slice(0, 2));
    expect(e.getAllEntityTypes()).toEqual(['Task', 'Project']);
  });

  it('takes its row in one object: the fields, and the id and creation time it is given', () => {
    const id = e.create('Task', { id: 'Task-custom' as Id, createdAt: 42, title: 'Mine', tags: ['a'] }).id();
    expect(id).toBe('Task-custom');
    expect(e.findById(id)).toEqual({ id, entityType: 'Task', createdAt: 42, updatedAt: 42, title: 'Mine', tags: ['a'] });
  });

  it("refuses a row naming the engine's other fields, before the entity exists", () => {
    for (const field of ['entityType', 'role', 'trashedAt', 'relationDetails']) {
      expect(() => e.create('Task', { title: 'x', [field]: 1 }), field).toThrow(expect.objectContaining({ code: 'RESERVED_ATTRIBUTE' }));
    }
    expect(e.admin.getAllEntities()).toEqual([]);
  });

  it('refuses a name that is not a registered type, an id in use, and an id naming another type', () => {
    expect(() => e.create('Memo' as never)).toThrow(expect.objectContaining({ code: 'UNKNOWN_ENTITY_TYPE' }));
    expect(() => e.create('Task-1' as never)).toThrow(/looks like an id: use write\(id\)/);
    const id = e.create('Task').id();
    expect(() => e.create('Task', { id })).toThrow(expect.objectContaining({ code: 'ID_IN_USE' }));
    expect(() => e.create('Task', { id: 'Project-1' as Id })).toThrow(expect.objectContaining({ code: 'TYPE_MISMATCH' }));
    expect(e.admin.getAllEntities()).toEqual([id]);
  });

  it('createEntityWithDefaults fills in a short code, a label and timestamps', () => {
    const created = e.createEntityWithDefaults('Project', { color: 'red' });
    expect(created).toEqual({
      id: expect.stringMatching(/^Project-/), color: 'red', entityType: 'Project',
      shortCode: 'PRO-1', label: 'New Project 1', createdAt: NOW, updatedAt: NOW,
    });
    expect(e.findById(created.id)).toEqual({ ...created });
    const second = e.createEntityWithDefaults('Project', { label: 'Named', id: 'Project-given' as Id }, 'PX');
    expect(second).toMatchObject({ id: 'Project-given', shortCode: 'PX-1', label: 'Named' });
    expect(e.generateShortCode('Project', 'P')).toBe('P-1');
    expect(e.generateLabelWithCount('Board', 'Project')).toBe('Board 1');
  });
});

describe('numbering', () => {
  it('never hands a short code out twice, though the row holding the highest is destroyed', () => {
    const first = e.createEntityWithDefaults('Project', {});
    const second = e.createEntityWithDefaults('Project', {});
    expect(second.shortCode).toBe('PRO-2');
    e.write(second.id).destroy();
    expect(e.createEntityWithDefaults('Project', {}).shortCode).toBe('PRO-3');
    e.write(first.id).destroy();
    expect(e.createEntityWithDefaults('Project', {}).shortCode).toBe('PRO-4');
  });

  it('starts after the highest number a type already holds, and skips one a row took for itself', () => {
    e.create('Task').set('shortCode', 'TSK-7');
    e.create('Task').set('shortCode', 'TSK-x');
    e.create('Task').set('label', 'New Task 2');
    expect(e.generateShortCode('Task', 'TSK')).toBe('TSK-8');
    e.create('Task').set('shortCode', 'TSK-9');
    expect(e.generateShortCode('Task', 'TSK')).toBe('TSK-10');
    expect(e.generateLabelWithCount('New Task', 'Task')).toBe('New Task 3');
  });

  it('gives back the numbers a unit took when it throws', () => {
    expect(() => e.transact(() => {
      e.generateShortCode('Task', 'TSK');
      e.generateShortCode('Task', 'TSK');
      throw new Error('stop');
    })).toThrow('stop');
    expect(e.generateShortCode('Task', 'TSK')).toBe('TSK-1');
  });

  it('keeps its sequences out of query.all()', () => {
    const id = e.createEntityWithDefaults('Task', {}).id;
    expect(e.query.all().ids()).toEqual([id]);
  });
});

describe('write', () => {
  it('writes to an existing entity, leaving its createdAt and stamping updatedAt', () => {
    const id = e.create('Task').id();
    vi.setSystemTime(NOW + 5);
    e.write(id).set('title', 'Later');
    expect(e.findById(id)).toEqual({ id, entityType: 'Task', createdAt: NOW, updatedAt: NOW + 5, title: 'Later' });
  });

  /** An id no entity has used to be written to anyway, leaving attributes with no entity of a type behind them */
  it('refuses an id no entity has, writing nothing', () => {
    expect(() => e.write('Task-missing' as Id)).toThrow(expect.objectContaining({ code: 'NO_SUCH_ENTITY' }));
    const gone = e.create('Task').id();
    e.write(gone).destroy();
    expect(() => e.write(gone).set('title', 'back')).toThrow(/no entity has this id/);
    expect(e.admin.getAllEntities()).toEqual([]);
  });
});

describe('required fields', () => {
  const strict = () => freshEngine({ isEntityType, isRelationKind, isRole, requiredFields: (type) => (type === 'Task' ? ['title', 'done'] : []) });

  it('refuses a created row without a field its type requires, before the entity exists', () => {
    const e = strict();
    expect(() => e.create('Task', { title: 'x' })).toThrow(expect.objectContaining({ code: 'REQUIRED_FIELD', message: expect.stringContaining('"done"') }));
    expect(() => e.create('Task', { title: 'x', done: undefined })).toThrow(expect.objectContaining({ code: 'REQUIRED_FIELD' }));
    expect(e.admin.getAllEntities()).toEqual([]);
    // null is a value, so a field holding it is held
    const id = e.create('Task', { title: 'x', done: null }).id();
    expect(e.getRow(id)).toMatchObject({ title: 'x', done: null });
    expect(e.create('Project').id()).toMatch(/^Project-/);
  });

  it('refuses deleting a required field, which set replaces instead', () => {
    const e = strict();
    const id = e.create('Task', { title: 'x', done: false, note: 'n' }).id();
    expect(() => e.write(id).delete('title')).toThrow(expect.objectContaining({ code: 'REQUIRED_FIELD' }));
    e.write(id).delete('note').set('title', 'y');
    expect(e.getRow(id)).toMatchObject({ title: 'y', done: false });
  });

  it("loads a row through the admin face whatever it holds, which is how data a shape's change left is read", () => {
    const e = strict();
    e.admin.putAttr('Task-old' as Id, 'title', 'kept');
    expect(e.getAttr('Task-old' as Id, 'title')).toBe('kept');
  });
});

describe('upsert', () => {
  it('creates the entity with the id and its row when none has it, and writes the row to it when one does', () => {
    const id = 'Task-singleton' as Id;
    e.upsert('Task', id, { title: 'first' });
    vi.setSystemTime(NOW + 5);
    e.upsert('Task', id, { title: 'second' }).set('done', true);
    expect(e.findById(id)).toEqual({ id, entityType: 'Task', createdAt: NOW, updatedAt: NOW + 5, title: 'second', done: true });
    // The id is upsert's argument, never a field of its row
    expect(() => e.upsert('Task', id, { id })).toThrow(expect.objectContaining({ code: 'RESERVED_ATTRIBUTE' }));
  });

  it('refuses an unregistered type, and an entity of another type', () => {
    expect(() => e.upsert('Memo' as never, 'Memo-1' as Id)).toThrow(expect.objectContaining({ code: 'UNKNOWN_ENTITY_TYPE' }));
    const id = e.create('Task').id();
    expect(() => e.upsert('Project', id)).toThrow(expect.objectContaining({ code: 'TYPE_MISMATCH' }));
  });
});

describe('attribute writes', () => {
  it('set replaces, add appends to a list, setMany sets each; a read gives back exactly what was written', () => {
    const id = e.create('Task').set('tag', ['a']).id();
    e.write(id).add('tag', 'b').add('tag', 'c');
    expect(e.getAttr(id, 'tag')).toEqual(['a', 'b', 'c']);
    expect(e.findById(id)).toMatchObject({ tag: ['a', 'b', 'c'] });
    e.write(id).set('tag', 'z');
    expect(e.getAttr(id, 'tag')).toBe('z');
    e.write(id).setMany({ tag: 'x', other: 1 });
    expect(e.findById(id)).toEqual({ id, entityType: 'Task', createdAt: NOW, updatedAt: NOW, tag: 'x', other: 1 });
  });

  it('a one-element list reads as a list, whichever write made it', () => {
    const id = e.create('Task').set('written', ['only']).add('added', 'only').id();
    expect(e.getRow(id)).toMatchObject({ written: ['only'], added: ['only'] });
    expect(e.query('Task').pickAll()[0]).toMatchObject({ written: ['only'], added: ['only'] });
  });

  it('add, deleteAt and deleteValue refuse a value that is not a list', () => {
    const id = e.create('Task').set('title', 'one').id();
    for (const attempt of [() => e.write(id).add('title', 'two'), () => e.write(id).deleteAt('title', 0), () => e.write(id).deleteValue('title', 'one')]) {
      expect(attempt).toThrow(expect.objectContaining({ code: 'NOT_A_LIST' }));
    }
    expect(e.getAttr(id, 'title')).toBe('one');
  });

  it('merge combines objects, and sets the value when either is not one', () => {
    const id = e.create('Task').set('meta', { a: 1, b: 1 }).id();
    e.write(id).merge('meta', { b: 2, c: 3 });
    expect(e.getAttr(id, 'meta')).toEqual({ a: 1, b: 2, c: 3 });
    e.write(id).merge('meta', 'plain');
    expect(e.getAttr(id, 'meta')).toBe('plain');
    e.write(id).merge('fresh', { v: 1 });
    expect(e.getAttr(id, 'fresh')).toEqual({ v: 1 });
  });

  it('deleteAt removes by index, deleteValue by value or by matching object fields, delete the attribute', () => {
    const id = e.create('Task').set('tag', ['a', 'b', { k: 1, x: 2 }, 'c']).id();
    e.write(id).deleteAt('tag', 1);
    expect(e.getAttr(id, 'tag')).toEqual(['a', { k: 1, x: 2 }, 'c']);
    e.write(id).deleteValue('tag', { k: 1 });
    expect(e.getAttr(id, 'tag')).toEqual(['a', 'c']);
    e.write(id).deleteValue('tag', 'missing').deleteAt('tag', 5).delete('absent');
    expect(e.getAttr(id, 'tag')).toEqual(['a', 'c']);
    e.write(id).deleteValue('tag', 'a').deleteValue('tag', 'c');
    expect(e.getAttr(id, 'tag')).toEqual([]);
    e.write(id).delete('tag');
    expect(e.getAttr(id, 'tag')).toBeNull();
    expect(e.findById(id)).toEqual({ id, entityType: 'Task', createdAt: NOW, updatedAt: NOW });
  });

  it('never changes a list a read handed out', () => {
    const id = e.create('Task').set('tag', ['a']).id();
    const read = e.getAttr(id, 'tag');
    e.write(id).add('tag', 'b').deleteAt('tag', 0);
    expect(read).toEqual(['a']);
  });

  it.each(['id', 'entityType', 'createdAt', 'role', 'relationDetails'])('refuses %s, which only the engine writes', (name) => {
    const id = e.create('Task').id();
    const before = e.getRow(id);
    const w = () => e.write(id);
    for (const attempt of [
      () => w().set(name, 'x'), () => w().add(name, 'x'), () => w().merge(name, {}), () => w().delete(name),
      () => w().deleteAt(name, 0), () => w().deleteValue(name, 'x'), () => w().setMany({ title: 'kept out', [name]: 'x' }),
    ]) {
      expect(attempt).toThrow(expect.objectContaining({ code: 'RESERVED_ATTRIBUTE' }));
    }
    // setMany refused before writing any field
    expect(e.getRow(id)).toEqual(before);
  });

  it('null is a value, undefined leaves an attribute alone in setMany and is refused by set', () => {
    const id = e.create('Task').setMany({ title: 'A', gone: 1 }).id();
    e.write(id).setMany({ title: null, gone: undefined, list: [3] });
    expect(e.getRow(id)).toMatchObject({ title: null, gone: 1, list: [3] });
    expect(() => e.write(id).set('title', undefined)).toThrow(expect.objectContaining({ code: 'NO_VALUE' }));
    e.write(id).delete('gone');
    expect(e.getRow(id)).not.toHaveProperty('gone');
  });
});

describe('updatedAt', () => {
  const later = () => vi.setSystemTime(NOW + 10);

  it('is the creation time on create, or the one given', () => {
    const id = e.create('Task').set('title', 'A').id();
    expect(e.getAttr(id, 'updatedAt')).toBe(NOW);
    const imported = e.create('Task', { createdAt: 5, updatedAt: 9 }).id();
    expect(e.getRow(imported)).toMatchObject({ createdAt: 5, updatedAt: 9 });
  });

  it.each([
    ['set', (w: WriteBuilder<'Task'>) => w.set('title', 'B')],
    ['setMany', (w: WriteBuilder<'Task'>) => w.setMany({ title: 'B' })],
    ['add', (w: WriteBuilder<'Task'>) => w.add('tags', 'x')],
    ['merge', (w: WriteBuilder<'Task'>) => w.merge('meta', { k: 1 })],
    ['delete', (w: WriteBuilder<'Task'>) => w.delete('title')],
    ['deleteAt', (w: WriteBuilder<'Task'>) => w.deleteAt('tags', 0)],
    ['deleteValue', (w: WriteBuilder<'Task'>) => w.deleteValue('tags', 'a')],
  ])('is stamped by %s', (_name, change) => {
    const id = e.create('Task').setMany({ title: 'A', tags: ['a'] }).id();
    later();
    change(e.write(id));
    expect(e.getAttr(id, 'updatedAt')).toBe(NOW + 10);
  });

  it('is not stamped by links, roles, the trash or a write that asked not to touch it', () => {
    const [id, other] = [e.create('Task').id(), e.create('Task').id()];
    later();
    e.write(id).link('blocks', other).grant('lead').trash().restore();
    e.write(id, { touch: false }).set('lastSeen', 1);
    expect(e.getAttr(id, 'updatedAt')).toBe(NOW);
  });

  it('is written once for a builder and only by the engine', () => {
    const { sink, calls } = recordingSink();
    const engineWithSink = engine(sink);
    const id = engineWithSink.create('Task').id();
    calls.length = 0;
    engineWithSink.write(id).set('a', 1).set('b', 2).add('c', 3);
    expect(calls.filter(([, kind]) => kind === 'updatedAt')).toHaveLength(1);
    expect(() => e.write(e.create('Task').id()).set('updatedAt', 1)).toThrow(expect.objectContaining({ code: 'RESERVED_ATTRIBUTE' }));
  });
});

describe('destroy', () => {
  it('removes the entity, its attributes and its relations', () => {
    const a = e.create('Task').set('title', 'A').id();
    const b = e.create('Task').set('title', 'B').id();
    const c = e.create('Project').id();
    e.write(a).link('blocks', b);
    e.write(c).link('contains', a);
    e.write(c).link('contains', b);
    expect(e.write(a).destroy()).toBeUndefined();
    expect(e.findById(a)).toBeUndefined();
    expect(e.getAttr(a, 'title')).toBeNull();
    expect(e.admin.getEntitiesOfType('Task')).toEqual([b]);
    expect(e.findRelations().map((r) => [r.sourceEntity, r.relationType, r.targetEntity])).toEqual([[c, 'contains', b]]);
    expect(e.getAllRelationKinds()).toEqual(['contains']);
    expect(e.query.ids(c).out('contains').ids()).toEqual([b]);
  });

  it('destroy forgets an emptied type', () => {
    const a = e.create('Person').id();
    e.write(a).destroy();
    expect(e.getAllEntityTypes()).toEqual([]);
    expect(e.admin.getAllEntities()).toEqual([]);
  });
});

describe('trash', () => {
  it('takes an entity out of every query, a link\'s far end included, and withTrashed and trashed reach it', () => {
    const kept = e.create('Task').set('title', 'kept').id();
    const binned = e.create('Task').set('title', 'binned').id();
    const project = e.create('Project').id();
    e.write(project).link('contains', kept).link('contains', binned);
    e.write(binned).trash(5);

    expect(e.query('Task').ids()).toEqual([kept]);
    expect(e.query.ids([kept, binned]).ids()).toEqual([kept]);
    expect(e.query.all().ofType('Task').ids()).toEqual([kept]);
    expect(e.query.ids(project).out('contains').ids()).toEqual([kept]);
    expect(e.query.ids(project).outEdges('contains').map(({ id }) => id)).toEqual([kept]);
    expect(e.query.withTrashed('Task').ids()).toEqual([kept, binned]);
    expect(e.query.withTrashed.ids(project).out('contains').ids()).toEqual([kept, binned]);
    expect(e.query.trashed('Task').pickAll()).toEqual([expect.objectContaining({ id: binned, title: 'binned', trashedAt: 5 })]);
    // Its edges stay, and a write still reaches it
    expect(e.findRelations({ targetEntity: binned })).toHaveLength(1);
    e.write(binned).set('title', 'still writable');

    e.write(binned).restore();
    expect(e.query('Task').ids()).toEqual([kept, binned]);
    expect(e.query.trashed('Task').ids()).toEqual([]);
    expect(e.getRow(binned)).not.toHaveProperty('trashedAt');
  });

  it('keeps the first time when trashed again, and refuses trashedAt as an attribute write', () => {
    const id = e.create('Task').trash(1).trash(2).id();
    expect(e.getAttr(id, 'trashedAt')).toBe(1);
    expect(() => e.write(id).set('trashedAt', 3)).toThrow(expect.objectContaining({ code: 'RESERVED_ATTRIBUTE' }));
  });

  it('drops a trashed row from a query kept across the trash, and a unit that throws keeps it out of the trash', () => {
    const id = e.create('Task').id();
    const kept = e.query('Task');
    e.write(id).trash();
    expect(kept.limit(5).ids()).toEqual([]);
    e.write(id).restore();
    expect(() => e.transact(() => { e.write(id).trash(); throw new Error('stop'); })).toThrow('stop');
    expect(e.query('Task').ids()).toEqual([id]);
  });
});

describe('rows', () => {
  it('a row is the id, the type and every attribute; none for an id no entity has; a trashed one all the same', () => {
    const id = e.create('Task').set('title', 'A').grant('lead').id();
    const row = { id, entityType: 'Task', createdAt: NOW, updatedAt: NOW, title: 'A', role: ['lead'] };
    expect(e.getRow(id)).toEqual(row);
    expect(e.query.ids(id).pickAll()).toEqual([row]);
    expect(e.findById(id)).toEqual(row);
    expect(e.getRow('Task-none' as Id)).toBeUndefined();
    e.write(id).trash(7);
    expect(e.getRow(id)).toEqual({ ...row, trashedAt: 7 });
    expect(e.findById(id)).toBeUndefined();
  });
});

describe('finders', () => {
  it('skip rows in the trash', () => {
    const a = e.create('Task').setMany({ title: 'A', status: 'open' }).id();
    const b = e.create('Task').setMany({ title: 'B', status: 'open' }).trash().id();
    const c = e.create('Task').setMany({ title: 'C', status: 'done' }).id();
    e.write(c).grant('pinned');
    e.write(b).grant('pinned');
    expect(e.findById(b)).toBeUndefined();
    expect(e.findAll<{ id: Id }>('Task').map((r) => r.id)).toEqual([a, c]);
    expect(e.findWhere<{ id: Id }>('Task', 'status', 'open').map((r) => r.id)).toEqual([a]);
  });

  it('read every field of a row, attribute kinds and stats', () => {
    const a = e.create('Task').set('title', 'A').set('tag', [1, 2]).id();
    const p = e.create('Project').id();
    e.write(p).link('contains', a);
    expect(e.getRow(a)).toEqual({ id: a, entityType: 'Task', createdAt: NOW, updatedAt: NOW, title: 'A', tag: [1, 2] });
    // `entityType` is read from the id, so it is on every row and in every read, and stored as no attribute
    expect(e.getAttr(a, 'entityType')).toBe('Task');
    expect(e.getAttr(p, 'entityType')).toBe('Project');
    expect(e.query.all().where('entityType', 'Task').ids()).toEqual([a]);
    expect(e.getAttr('Task-gone' as Id, 'entityType')).toBeNull();
    expect(e.getAllAttributeKinds()).toEqual(['createdAt', 'updatedAt', 'title', 'tag', 'relationDetails']);
    expect(e.getAttributeStats('tag')).toEqual({ entityCount: 1, totalValues: 2 });
    expect(e.getSchemaStats()).toEqual({
      entities: { Task: 1, Project: 1, Relation: 1 },
      attributes: {
        createdAt: { entityCount: 2, totalValues: 2 }, updatedAt: { entityCount: 2, totalValues: 2 },
        title: { entityCount: 1, totalValues: 1 },
        tag: { entityCount: 1, totalValues: 2 }, relationDetails: { entityCount: 1, totalValues: 1 },
      },
      relations: { contains: { totalRelations: 1, uniqueSources: 1, uniqueTargets: 1 } },
    });
  });

  it('isEntityType answers from the engine\'s checker', () => {
    expect(e.isEntityType('Task')).toBe(true);
    expect(e.isEntityType('Memo')).toBe(false);
  });
});

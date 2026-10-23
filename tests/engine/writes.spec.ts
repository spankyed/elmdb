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
    expect(e.query.ids(id).pickOne()).toEqual({ id, entityType: 'Task', createdAt: NOW, updatedAt: NOW, title: 'Write specs' });
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
    expect(e.query.ids(id).pickOne()).toEqual({ id, entityType: 'Task', createdAt: 42, updatedAt: 42, title: 'Mine', tags: ['a'] });
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

  it('numbers a row of a type with a short code prefix that brings none, and keeps one it brings', () => {
    const e = numbered();
    const id = e.create('Project', { color: 'red' }).id();
    expect(e.query.ids(id).pickOne()).toEqual({ id, entityType: 'Project', color: 'red', shortCode: 'PRO-1', createdAt: NOW, updatedAt: NOW });
    expect(e.getRow(e.create('Project', { shortCode: 'PRO-40' }).id())?.shortCode).toBe('PRO-40');
    // A type with no prefix is not numbered
    expect(e.getRow(e.create('Task', { shortCode: undefined, title: 'x' }).id())).not.toHaveProperty('shortCode');
  });

  it('takes no number for a create it refuses', () => {
    const e = numbered();
    expect(() => e.create('Project', { id: 'Task-1' as Id })).toThrow(expect.objectContaining({ code: 'TYPE_MISMATCH' }));
    expect(() => e.create('Person', {})).toThrow(expect.objectContaining({ code: 'REQUIRED_FIELD', message: expect.stringContaining('"title"') }));
    expect(e.getRow(e.create('Project').id())?.shortCode).toBe('PRO-1');
    expect(e.getRow(e.create('Person', { title: 'x' }).id())?.shortCode).toBe('P-1');
  });
});

/** An engine whose Project rows are numbered `PRO-<n>` and Person rows `P-<n>`, a Person requiring its short code and title */
const numbered = () => freshEngine({
  isEntityType, isRelationKind, isRole,
  rowRules: (type) => ({ Project: { required: [], shortCode: 'PRO' }, Person: { required: ['shortCode', 'title'], shortCode: 'P' } })[type],
});
const code = (e: EngineUnderTest, fields: Record<string, unknown> = {}) => e.getRow(e.create('Project', fields).id())?.shortCode;

describe('numbering', () => {
  it('never hands a short code out twice, though the row holding the highest is destroyed', () => {
    const e = numbered();
    const first = e.create('Project').id();
    const second = e.create('Project').id();
    expect(e.getRow(second)?.shortCode).toBe('PRO-2');
    e.write(second).destroy();
    expect(code(e)).toBe('PRO-3');
    e.write(first).destroy();
    expect(code(e)).toBe('PRO-4');
  });

  it('starts after the highest number a type already holds, and skips one a row took for itself', () => {
    const e = numbered();
    e.create('Project', { shortCode: 'PRO-7' });
    e.create('Project', { shortCode: 'PRO-x' });
    expect(code(e)).toBe('PRO-8');
    e.create('Project', { shortCode: 'PRO-9' });
    expect(code(e)).toBe('PRO-10');
  });

  it('numbers a default label per type and base the same way', () => {
    e.create('Task').set('label', 'New Task 2');
    expect(e.generateLabelWithCount('New Task', 'Task')).toBe('New Task 3');
    expect(e.generateLabelWithCount('Board', 'Project')).toBe('Board 1');
  });

  it('gives back the numbers a unit took when it throws', () => {
    const e = numbered();
    expect(() => e.transact(() => {
      e.create('Project');
      e.create('Project');
      throw new Error('stop');
    })).toThrow('stop');
    expect(code(e)).toBe('PRO-1');
  });

  it('keeps its sequences out of query.all()', () => {
    const e = numbered();
    const id = e.create('Project').id();
    expect(e.query.all().ids()).toEqual([id]);
  });
});

describe('write', () => {
  it('writes to an existing entity, leaving its createdAt and stamping updatedAt', () => {
    const id = e.create('Task').id();
    vi.setSystemTime(NOW + 5);
    e.write(id).set('title', 'Later');
    expect(e.query.ids(id).pickOne()).toEqual({ id, entityType: 'Task', createdAt: NOW, updatedAt: NOW + 5, title: 'Later' });
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
  const strict = () => freshEngine({ isEntityType, isRelationKind, isRole, rowRules: (type) => (type === 'Task' ? { required: ['title', 'done'] } : undefined) });

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
    expect(e.getRow('Task-old' as Id)?.title).toBe('kept');
  });
});

describe('upsert', () => {
  it('creates the entity with the id and its row when none has it, and writes the row to it when one does', () => {
    const id = 'Task-singleton' as Id;
    e.upsert('Task', id, { title: 'first' });
    vi.setSystemTime(NOW + 5);
    e.upsert('Task', id, { title: 'second' }).set('done', true);
    expect(e.query.ids(id).pickOne()).toEqual({ id, entityType: 'Task', createdAt: NOW, updatedAt: NOW + 5, title: 'second', done: true });
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
    expect(e.getRow(id)?.tag).toEqual(['a', 'b', 'c']);
    expect(e.query.ids(id).pickOne()).toMatchObject({ tag: ['a', 'b', 'c'] });
    e.write(id).set('tag', 'z');
    expect(e.getRow(id)?.tag).toBe('z');
    e.write(id).setMany({ tag: 'x', other: 1 });
    expect(e.query.ids(id).pickOne()).toEqual({ id, entityType: 'Task', createdAt: NOW, updatedAt: NOW, tag: 'x', other: 1 });
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
    expect(e.getRow(id)?.title).toBe('one');
  });

  it('merge combines objects, and sets the value when either is not one', () => {
    const id = e.create('Task').set('meta', { a: 1, b: 1 }).id();
    e.write(id).merge('meta', { b: 2, c: 3 });
    expect(e.getRow(id)?.meta).toEqual({ a: 1, b: 2, c: 3 });
    e.write(id).merge('meta', 'plain');
    expect(e.getRow(id)?.meta).toBe('plain');
    e.write(id).merge('fresh', { v: 1 });
    expect(e.getRow(id)?.fresh).toEqual({ v: 1 });
  });

  it('deleteAt removes by index, deleteValue by value or by matching object fields, delete the attribute', () => {
    const id = e.create('Task').set('tag', ['a', 'b', { k: 1, x: 2 }, 'c']).id();
    e.write(id).deleteAt('tag', 1);
    expect(e.getRow(id)?.tag).toEqual(['a', { k: 1, x: 2 }, 'c']);
    e.write(id).deleteValue('tag', { k: 1 });
    expect(e.getRow(id)?.tag).toEqual(['a', 'c']);
    e.write(id).deleteValue('tag', 'missing').deleteAt('tag', 5).delete('absent');
    expect(e.getRow(id)?.tag).toEqual(['a', 'c']);
    e.write(id).deleteValue('tag', 'a').deleteValue('tag', 'c');
    expect(e.getRow(id)?.tag).toEqual([]);
    e.write(id).delete('tag');
    expect(e.getRow(id)?.tag).toBeUndefined();
    expect(e.query.ids(id).pickOne()).toEqual({ id, entityType: 'Task', createdAt: NOW, updatedAt: NOW });
  });

  it('never changes a list a read handed out', () => {
    const id = e.create('Task').set('tag', ['a']).id();
    const read = e.getRow(id)?.tag;
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
    expect(e.getRow(id)?.updatedAt).toBe(NOW);
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
    expect(e.getRow(id)?.updatedAt).toBe(NOW + 10);
  });

  it('is not stamped by links, roles, the trash or a write that asked not to touch it', () => {
    const [id, other] = [e.create('Task').id(), e.create('Task').id()];
    later();
    e.write(id).link('blocks', other).grant('lead').trash().restore();
    e.write(id, { touch: false }).set('lastSeen', 1);
    expect(e.getRow(id)?.updatedAt).toBe(NOW);
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
    expect(e.query.ids(a).pickOne()).toBeUndefined();
    expect(e.getRow(a)?.title).toBeUndefined();
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
    expect(e.getRow(id)?.trashedAt).toBe(1);
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
    expect(e.query.ids(id).pickOne()).toEqual(row);
    expect(e.getRow('Task-none' as Id)).toBeUndefined();
    e.write(id).trash(7);
    expect(e.getRow(id)).toEqual({ ...row, trashedAt: 7 });
    expect(e.query.ids(id).pickOne()).toBeUndefined();
  });
});

describe('rows', () => {
  it("pickOne() is a query's first whole row, which skips the trash where getRow reads it", () => {
    const a = e.create('Task').setMany({ title: 'A', status: 'open' }).id();
    const b = e.create('Task').setMany({ title: 'B', status: 'open' }).trash().id();
    expect(e.query.ids(a).pickOne()).toEqual(e.getRow(a));
    expect(e.query.ids(b).pickOne()).toBeUndefined();
    expect(e.getRow(b)).toMatchObject({ id: b, title: 'B' });
    expect(e.query('Task').pickOne()).toEqual(e.getRow(a));
    expect(e.query('Project').pickOne()).toBeUndefined();
    // With fields, the fields picked
    expect(e.query.ids(a).pickOne(['title'])).toEqual({ id: a, title: 'A' });
  });

  it("row() is a write's entity as its writes left it, a create's included", () => {
    const id = e.create('Task', { title: 'A' }).set('status', 'open').id();
    expect(e.create('Task', { title: 'B' }).row()).toMatchObject({ entityType: 'Task', title: 'B', createdAt: NOW, updatedAt: NOW });
    expect(e.write(id).set('title', 'A2').row()).toEqual({ id, entityType: 'Task', title: 'A2', status: 'open', createdAt: NOW, updatedAt: NOW });
    // A trashed entity's row is read as written, trashedAt included
    expect(e.write(id).trash(9).row()).toMatchObject({ id, trashedAt: 9 });
  });

  it('read every field of a row, attribute kinds and stats', () => {
    const a = e.create('Task').set('title', 'A').set('tag', [1, 2]).id();
    const p = e.create('Project').id();
    e.write(p).link('contains', a);
    expect(e.getRow(a)).toEqual({ id: a, entityType: 'Task', createdAt: NOW, updatedAt: NOW, title: 'A', tag: [1, 2] });
    // `entityType` is read from the id, so it is on every row and in every read, and stored as no attribute
    expect(e.getRow(a)?.entityType).toBe('Task');
    expect(e.getRow(p)?.entityType).toBe('Project');
    expect(e.query.all().where('entityType', 'Task').ids()).toEqual([a]);
    expect(e.getRow('Task-gone' as Id)?.entityType).toBeUndefined();
    expect(e.getAllAttributeKinds()).toEqual(['createdAt', 'updatedAt', 'title', 'tag', 'relationDetails']);
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

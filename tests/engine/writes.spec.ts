// The engine's contract: create, upsert and write create, change and delete entities, each held to the claim it
// makes about its argument, with the ids and fields they return
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { engine, NAMES, type Id } from './helpers.ts';
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
    // Stats are keyed by name, in name order
    expect(Object.keys(e.getSchemaStats().entities)).toEqual(['Project', 'Task']);
  });

  it('takes its row in one object: the fields, and the id and creation time it is given', () => {
    const id = e.create('Task', { id: 'Task-custom' as Id, createdAt: 42, title: 'Mine', tags: ['a'] }).id();
    expect(id).toBe('Task-custom');
    expect(e.query.ids(id).pickOne()).toEqual({ id, entityType: 'Task', createdAt: 42, updatedAt: 42, title: 'Mine', tags: ['a'] });
  });

  it("refuses a row naming the engine's other fields, before the entity exists", () => {
    for (const field of ['entityType', 'role', 'trashedAt']) {
      expect(() => e.create('Task', { title: 'x', [field]: 1 }), field).toThrow(expect.objectContaining({ code: 'RESERVED_ATTRIBUTE' }));
    }
    expect(e.admin.getAllEntities()).toEqual([]);
  });

  it("refuses the engine's own types, which its ids are written with, which no schema may declare", () => {
    const everything = engine();
    for (const type of ['Relation']) {
      expect(() => everything.create(type), type).toThrow(expect.objectContaining({ code: 'RESERVED_ENTITY_TYPE' }));
      expect(() => everything.upsert(type, { id: `${type}-1` as Id }), type).toThrow(expect.objectContaining({ code: 'RESERVED_ENTITY_TYPE' }));
    }
    expect(everything.admin.getAllEntities()).toEqual([]);
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

  it("refuses a short code that isn't the type's next form, one in use, and one for a type that numbers none", () => {
    const e = numbered();
    e.create('Project', { shortCode: 'PRO-3' });
    for (const shortCode of ['PRO-x', 'PRO-0', 'PRO-03', 'P-3', 'DOC-3', 3]) {
      expect(() => e.create('Project', { shortCode } as never), String(shortCode)).toThrow(expect.objectContaining({ code: 'INVALID_SHORT_CODE' }));
    }
    expect(() => e.create('Project', { shortCode: 'PRO-3' })).toThrow(expect.objectContaining({ code: 'SHORT_CODE_IN_USE' }));
    expect(() => e.create('Task', { shortCode: 'TSK-1', title: 'x' } as never)).toThrow(expect.objectContaining({ code: 'INVALID_SHORT_CODE' }));
    expect(e.query('Project').count()).toBe(1);
  });

  it('keeps a short code once written: no write changes or removes it, and an upsert may only repeat it', () => {
    const e = numbered();
    const id = e.create('Project').id();
    expect(() => e.write(id).set('shortCode', 'PRO-9')).toThrow(expect.objectContaining({ code: 'RESERVED_ATTRIBUTE' }));
    expect(() => e.write(id).delete('shortCode')).toThrow(expect.objectContaining({ code: 'RESERVED_ATTRIBUTE' }));
    expect(() => e.write(id).setMany({ shortCode: 'PRO-9' })).toThrow(expect.objectContaining({ code: 'RESERVED_ATTRIBUTE' }));
    e.upsert('Project', { id, shortCode: 'PRO-1', color: 'blue' });
    expect(() => e.upsert('Project', { id, shortCode: 'PRO-9' })).toThrow(expect.objectContaining({ code: 'INVALID_SHORT_CODE' }));
    expect(e.getRow(id)).toMatchObject({ shortCode: 'PRO-1', color: 'blue' });
  });

  it('takes no number for a create it refuses', () => {
    const e = numbered();
    expect(() => e.create('Project', { id: 'Task-1' as Id })).toThrow(expect.objectContaining({ code: 'TYPE_MISMATCH' }));
    expect(() => e.create('Person', {})).toThrow(expect.objectContaining({ code: 'REQUIRED_FIELD', message: expect.stringContaining('"title"') }));
    expect(e.getRow(e.create('Project').id())?.shortCode).toBe('PRO-1');
    expect(e.getRow(e.create('Person', { title: 'x' }).id())?.shortCode).toBe('P-1');
  });
});

/** An engine whose Project rows are numbered `PRO-<n>` and Person rows `P-<n>`, a Person requiring its title */
const numbered = () => engine({ rowRules: { Project: { required: [], shortCode: 'PRO' }, Person: { required: ['title'], shortCode: 'P' } } });
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
    expect(code(e)).toBe('PRO-8');
    e.create('Project', { shortCode: 'PRO-9' });
    expect(code(e)).toBe('PRO-10');
  });

  it('numbers a default label per type and base the same way', () => {
    e.create('Task').set('label', 'New Task 2');
    expect(e.nextLabel('Task', 'New Task')).toBe('New Task 3');
    expect(e.nextLabel('Project', 'Board')).toBe('Board 1');
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
  const strict = () => engine({ rowRules: { Task: { required: ['title', 'done'] } } });

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
    e.upsert('Task', { id, title: 'first' });
    vi.setSystemTime(NOW + 5);
    e.upsert('Task', { id, title: 'second' }).set('done', true);
    expect(e.query.ids(id).pickOne()).toEqual({ id, entityType: 'Task', createdAt: NOW, updatedAt: NOW + 5, title: 'second', done: true });
    // A created row keeps the times it brings, as create's does; an entity that exists keeps its own
    const imported = 'Task-imported' as Id;
    e.upsert('Task', { id: imported, createdAt: 7, updatedAt: 8 });
    expect(e.query.ids(imported).pickOne()).toMatchObject({ createdAt: 7, updatedAt: 8 });
    expect(() => e.upsert('Task', { id, createdAt: 7 })).toThrow(expect.objectContaining({ code: 'RESERVED_ATTRIBUTE' }));
  });

  it('refuses an unregistered type, and an entity of another type', () => {
    expect(() => e.upsert('Memo' as never, { id: 'Memo-1' as Id })).toThrow(expect.objectContaining({ code: 'UNKNOWN_ENTITY_TYPE' }));
    const id = e.create('Task').id();
    expect(() => e.upsert('Project', { id })).toThrow(expect.objectContaining({ code: 'TYPE_MISMATCH' }));
  });
});

describe('attribute writes', () => {
  it('set replaces, add appends to a list, setMany sets each; a read gives back exactly what was written', () => {
    const id = e.create('Task').set('tag', ['a']).id();
    e.write(id).append('tag', 'b').append('tag', 'c');
    expect(e.getRow(id)?.tag).toEqual(['a', 'b', 'c']);
    expect(e.query.ids(id).pickOne()).toMatchObject({ tag: ['a', 'b', 'c'] });
    e.write(id).set('tag', 'z');
    expect(e.getRow(id)?.tag).toBe('z');
    e.write(id).setMany({ tag: 'x', other: 1 });
    expect(e.query.ids(id).pickOne()).toEqual({ id, entityType: 'Task', createdAt: NOW, updatedAt: NOW, tag: 'x', other: 1 });
  });

  it('a one-element list reads as a list, whichever write made it', () => {
    const id = e.create('Task').set('written', ['only']).append('added', 'only').id();
    expect(e.getRow(id)).toMatchObject({ written: ['only'], added: ['only'] });
    expect(e.query('Task').pickAll()[0]).toMatchObject({ written: ['only'], added: ['only'] });
  });

  it('append, removeAt and removeValue refuse a value that is not a list', () => {
    const id = e.create('Task').set('title', 'one').id();
    for (const attempt of [() => e.write(id).append('title', 'two'), () => e.write(id).removeAt('title', 0), () => e.write(id).removeValue('title', 'one')]) {
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

  it('removeAt removes by index, removeValue by value or by matching object fields, delete the field', () => {
    const id = e.create('Task').set('tag', ['a', 'b', { k: 1, x: 2 }, 'c']).id();
    e.write(id).removeAt('tag', 1);
    expect(e.getRow(id)?.tag).toEqual(['a', { k: 1, x: 2 }, 'c']);
    e.write(id).removeValue('tag', { k: 1 });
    expect(e.getRow(id)?.tag).toEqual(['a', 'c']);
    e.write(id).removeValue('tag', 'missing').removeAt('tag', 5).delete('absent');
    expect(e.getRow(id)?.tag).toEqual(['a', 'c']);
    e.write(id).removeValue('tag', 'a').removeValue('tag', 'c');
    expect(e.getRow(id)?.tag).toEqual([]);
    e.write(id).delete('tag');
    expect(e.getRow(id)?.tag).toBeUndefined();
    expect(e.query.ids(id).pickOne()).toEqual({ id, entityType: 'Task', createdAt: NOW, updatedAt: NOW });
  });

  it('never changes a list a read handed out', () => {
    const id = e.create('Task').set('tag', ['a']).id();
    const read = e.getRow(id)?.tag;
    e.write(id).append('tag', 'b').removeAt('tag', 0);
    expect(read).toEqual(['a']);
  });

  it.each(['id', 'entityType', 'createdAt', 'role'])('refuses %s, which only the engine writes', (name) => {
    const id = e.create('Task').id();
    const before = e.getRow(id);
    const w = () => e.write(id);
    for (const attempt of [
      () => w().set(name, 'x'), () => w().append(name, 'x'), () => w().merge(name, {}), () => w().delete(name),
      () => w().removeAt(name, 0), () => w().removeValue(name, 'x'), () => w().setMany({ title: 'kept out', [name]: 'x' }),
    ]) {
      expect(attempt).toThrow(expect.objectContaining({ code: 'RESERVED_ATTRIBUTE' }));
    }
    // setMany refused before writing any field
    expect(e.getRow(id)).toEqual(before);
  });

  it('null is a value, undefined leaves an attribute alone in setMany and is refused by every other attribute write', () => {
    const id = e.create('Task').setMany({ title: 'A', gone: 1 }).id();
    e.write(id).setMany({ title: null, gone: undefined, list: [3] });
    expect(e.getRow(id)).toMatchObject({ title: null, gone: 1, list: [3] });
    expect(() => e.write(id).set('title', undefined)).toThrow(expect.objectContaining({ code: 'NO_VALUE' }));
    for (const write of [() => e.write(id).append('tags', undefined), () => e.write(id).merge('meta', undefined as never), () => e.write(id).removeValue('tags', undefined)]) {
      expect(write).toThrow(expect.objectContaining({ code: 'NO_VALUE' }));
    }
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
    ['append', (w: WriteBuilder<'Task'>) => w.append('tags', 'x')],
    ['merge', (w: WriteBuilder<'Task'>) => w.merge('meta', { k: 1 })],
    ['delete', (w: WriteBuilder<'Task'>) => w.delete('title')],
    ['removeAt', (w: WriteBuilder<'Task'>) => w.removeAt('tags', 0)],
    ['removeValue', (w: WriteBuilder<'Task'>) => w.removeValue('tags', 'a')],
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

  it('is written once for a builder, at its first write, and only by the engine', () => {
    const id = e.create('Task').id();
    const builder = e.write(id).set('a', 1);
    later();
    builder.set('b', 2).append('c', 3);
    expect(e.getRow(id)?.updatedAt).toBe(NOW);
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
    expect(e.findRelations().map((r) => [r.source, r.kind, r.target])).toEqual([[c, 'contains', b]]);
    expect(Object.keys(e.getSchemaStats().relations)).toEqual(['contains']);
    expect(e.query.ids(c).out('contains').ids()).toEqual([b]);
  });

  it('destroy forgets an emptied type', () => {
    const a = e.create('Person').id();
    e.write(a).destroy();
    expect(Object.keys(e.getSchemaStats().entities)).toEqual([]);
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
    expect(e.query.ids(project).outEdges('contains').map(({ target }) => target)).toEqual([kept]);
    expect(e.query.withTrashed('Task').ids()).toEqual([kept, binned]);
    expect(e.query.withTrashed.ids(project).out('contains').ids()).toEqual([kept, binned]);
    expect(e.query.trashed('Task').pickAll()).toEqual([expect.objectContaining({ id: binned, title: 'binned', trashedAt: 5 })]);
    // The trash's entry points are the query's: by type, by ids (only those in the trash) and every trashed entity
    expect(e.query.trashed.ids([kept, binned]).ids()).toEqual([binned]);
    expect(e.query.trashed.ids(kept).exists()).toBe(false);
    expect(e.query.trashed.all().ids()).toEqual([binned]);
    // Its edges stay, and a write still reaches it
    expect(e.findRelations({ target: binned })).toHaveLength(1);
    e.write(binned).set('title', 'still writable');

    e.write(binned).restore();
    expect(e.query('Task').ids()).toEqual([kept, binned]);
    expect(e.query.trashed('Task').ids()).toEqual([]);
    expect(e.query.trashed.ids(binned).exists()).toBe(false);
    expect(e.query.trashed.all().ids()).toEqual([]);
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

  it('a field named __proto__ is a field like any other', () => {
    const id = e.create('Task').set('__proto__', { x: 1 }).id();
    expect(e.getAttr(id, '__proto__')).toEqual({ x: 1 });
    const row = e.getRow(id)!;
    expect(Object.getOwnPropertyDescriptor(row, '__proto__')?.value).toEqual({ x: 1 });
    expect(Object.getPrototypeOf(row)).toBe(Object.prototype);
    e.write(id).delete('__proto__');
    expect(e.getAttr(id, '__proto__')).toBeUndefined();
  });

  it('a value holding a __proto__ key reads back as stored, whichever read answers it', () => {
    const value = JSON.parse('{"__proto__": {"x": 1}, "a": 2}') as Record<string, unknown>;
    const holds = (read: unknown) => {
      expect(Object.keys(read as object)).toEqual(['__proto__', 'a']);
      expect(Object.getOwnPropertyDescriptor(read, '__proto__')?.value).toEqual({ x: 1 });
      expect(Object.getPrototypeOf(read)).toBe(Object.prototype);
    };
    const id = e.create('Task').set('meta', value).set('n', 1).id();
    const other = e.create('Task').id();
    e.write(id).link('refs', other, { info: value });
    holds(e.getRow(id)!.meta);
    holds(e.getAttr(id, 'meta'));
    holds(e.findRelations({ source: id })[0]!.info);
    // A warned engine has an unindexed `where` answered inside the count of rows it walked
    const warned = freshEngine({ ...NAMES, warn: () => {} });
    warned.create('Task').set('meta', value).set('n', 1);
    holds(warned.query('Task').where('n', 1).pickAll()[0]!.meta);
  });

  it('a schema naming a type __proto__ is refused for that name, which the database holds PascalCase', () => {
    // A computed key, since a literal's `__proto__:` sets the prototype
    const rules = { ['__proto__']: { required: ['title'], indexes: ['title'] } };
    expect(() => freshEngine({ types: ['__proto__'], relationKinds: [], roles: [], rowRules: rules }))
      .toThrow(/^The schema has 1 problem:\ntypes\.__proto__: a type's name is PascalCase, which "__proto__" isn't$/);
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

  it("getAttr is one field of the row, as getRow reads it: through the trash, undefined when unset, a stored null as null", () => {
    const id = e.create('Task', { title: 'A' }).set('status', null).id();
    expect(e.getAttr(id, 'title')).toBe('A');
    expect(e.getAttr(id, 'status')).toBeNull();
    expect(e.getAttr(id, 'nope')).toBeUndefined();
    expect(e.getAttr(id, 'entityType')).toBe('Task');
    e.write(id).trash(9);
    expect(e.getAttr(id, 'title')).toBe('A');
    expect(e.getAttr('Task-gone' as Id, 'title')).toBeUndefined();
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
    expect(Object.keys(e.getSchemaStats().attributes).sort()).toEqual(['createdAt', 'tag', 'title', 'updatedAt']);
    expect(e.getSchemaStats()).toEqual({
      entities: { Task: 1, Project: 1 },
      attributes: {
        createdAt: { entityCount: 2, totalValues: 2 }, updatedAt: { entityCount: 2, totalValues: 2 },
        title: { entityCount: 1, totalValues: 1 },
        tag: { entityCount: 1, totalValues: 2 },
      },
      relations: { contains: { total: 1, uniqueSources: 1, uniqueTargets: 1 } },
      roles: {},
    });
  });

  it('isEntityType answers from the engine\'s checker', () => {
    expect(e.isEntityType('Task')).toBe(true);
    expect(e.isEntityType('Memo')).toBe(false);
  });
});

describe('schema stats', () => {
  it('count a field named for an Object.prototype member, and change no other object', () => {
    e.create('Task').setMany({ constructor: 1, toString: 'x' }).set('__proto__', 5);
    const { attributes } = e.getSchemaStats();
    expect(Object.getOwnPropertyDescriptor(attributes, '__proto__')?.value).toEqual({ entityCount: 1, totalValues: 1 });
    expect(Object.getOwnPropertyDescriptor(attributes, 'constructor')?.value).toEqual({ entityCount: 1, totalValues: 1 });
    expect(Object.getOwnPropertyDescriptor(attributes, 'toString')?.value).toEqual({ entityCount: 1, totalValues: 1 });
    expect(({} as Record<string, unknown>).entityCount).toBeUndefined();
  });
});

describe("fields typed by a type's shape", () => {
  const typed = () => freshEngine({
    ...NAMES,
    rowRules: {
      Task: {
        required: ['title'],
        search: { fields: { body: 1 } },
        fields: {
          title: { type: 'string' },
          rank: { type: 'number', optional: true },
          stage: { type: 'enum', values: ['draft', 'final'], optional: true },
          icon: { type: 'string', optional: true, nullable: true },
          owner: { type: 'ref', to: ['Person'], optional: true },
          elsewhere: { type: 'ref', to: ['Nowhere'], optional: true },
          tags: { type: 'list', of: { type: 'string' }, optional: true },
          body: { type: 'enum', values: ['a'], optional: true },
        },
      },
    },
  });

  it('refuses a value its field does not take, from every write, naming the field and the type', () => {
    const e = typed();
    const id = e.create('Task', { title: 'one' }).id();
    for (const write of [
      () => e.write(id).set('rank', 'high'),
      () => e.write(id).set('stage', 'published'),
      () => e.write(id).set('owner', 'Project-p'),
      () => e.write(id).setMany({ tags: ['a', 2] }),
      () => e.write(id).set('title', null),
      () => e.create('Task', { title: 3 }),
      () => e.upsert('Task', { id: 'Task-u', title: 'u', rank: '1' }),
    ]) expect(write).toThrow(expect.objectContaining({ code: 'FIELD_TYPE' }));
    expect(() => e.write(id).set('rank', 'high')).toThrow(/Task\.rank is a number/);
    expect(e.getRow(id)).toMatchObject({ title: 'one' });
  });

  it('takes what the shape allows, null where it says so, and a field it does not declare', () => {
    const e = typed();
    const id = e.create('Task', { title: 'one', rank: 2, stage: 'final', icon: null, owner: 'Person-x', tags: ['a'] }).id();
    e.write(id).set('undeclared', { any: 'thing' }).set('icon', 'star');
    expect(e.getRow(id)).toMatchObject({ rank: 2, stage: 'final', icon: 'star', owner: 'Person-x', tags: ['a'], undeclared: { any: 'thing' } });
  });

  it('holds a ref to a type the schema does not declare as text, and a field its text index reads as anything', () => {
    const e = typed();
    const id = e.create('Task', { title: 'one', elsewhere: 'anything-at-all', body: 'not in the enum' }).id();
    expect(e.getRow(id)).toMatchObject({ elsewhere: 'anything-at-all', body: 'not in the enum' });
  });
});

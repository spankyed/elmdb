// The conversions: a format 1 environment keeps one record of each fact afterwards (format 2), a format 2 one keeps
// one row per attribute (format 3), a format 3 one is the engine's keys in one database (format 4), and none loses
// anything else
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { afterEach, expect, it } from 'vitest';
import { open } from 'lmdb';
import { compactEnvironment, convertToCurrentFormat, convertToFormat2, convertToFormat3, convertToFormat4, LMDB_FORMAT_VERSION, openLmdbStore, openRecordStore } from '../../src/lmdb/index.ts';
import { createEarsEngine, type EARS } from '../../src/index.ts';

const SEP = '\x1F';
const dirs: string[] = [];
afterEach(() => { for (const dir of dirs.splice(0)) fs.rmSync(dir, { recursive: true, force: true }); });

/** A format 1 environment as the stored data showed them: every doubled fact present once */
function format1(): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ears-format-1-'));
  dirs.push(dir);
  const root = open({ path: dir, maxDbs: 8, compression: true });
  const entities = root.openDB({ name: 'entities', encoding: 'json' });
  const attrs = root.openDB({ name: 'attrs', encoding: 'json' });
  const relations = root.openDB({ name: 'relations', encoding: 'json' });
  const attr = (kind: string, id: string, value: unknown, i = 0) => attrs.putSync(`${kind}${SEP}${id}${SEP}${i}`, { t: typeof value, v: value });
  // An entity with its row (flush-time createdAt) and its attributes, one of them the stored entityType
  entities.putSync('Note-1', { type: 'Note', createdAt: 2 });
  attr('createdAt', 'Note-1', 1);
  attr('title', 'Note-1', 'kept');
  attr('entityType', 'Note-1', 'Note');
  // A relation: its row, its entity row and its details as an attribute
  relations.putSync('Relation-1', { kind: 'refs', src: 'Note-1', tgt: 'Flow-1', info: null, createdAt: 3 });
  entities.putSync('Relation-1', { type: 'Relation', createdAt: 3 });
  attr('relationDetails', 'Relation-1', { sourceEntity: 'Note-1', targetEntity: 'Flow-1', relationType: 'refs' });
  // A row written for a relation's end that lives in another partition, and an entity with attributes and no row
  entities.putSync('Flow-1', { type: 'Flow', createdAt: 3 });
  attr('createdAt', 'Note-2', 4);
  root.close();
  return dir;
}

function read(dir: string) {
  const root = open({ path: dir, maxDbs: 8, compression: true, readOnly: true });
  try {
    const rows = (name: string) => Object.fromEntries([...root.openDB({ name, encoding: 'json' }).getRange()].map(({ key, value }) => [String(key), value]));
    return {
      entities: rows('entities'),
      attrs: rows('attrs') as Record<string, { t: string; v: unknown }>,
      attrKeys: Object.keys(rows('attrs')).sort(),
      relations: Object.keys(rows('relations')),
      format: root.openDB({ name: 'meta', encoding: 'json' }).get('format'),
    };
  } finally {
    root.close();
  }
}

it('removes the second record of each fact, keeps the first, and records format 2', () => {
  const dir = format1();
  expect(convertToFormat2(dir)).toEqual({
    entityTypeAttrs: 1, relationDetailAttrs: 1, relationRows: 1, orphanRows: 1, rewrittenRows: 1, addedRows: 1,
  });
  expect(read(dir)).toMatchObject({
    entities: { 'Note-1': { type: 'Note' }, 'Note-2': { type: 'Note' } },
    attrKeys: [`createdAt${SEP}Note-1${SEP}0`, `createdAt${SEP}Note-2${SEP}0`, `title${SEP}Note-1${SEP}0`],
    relations: ['Relation-1'],
    format: 2,
  });
});

it('refuses an environment that records another format than the step converts, changing nothing', () => {
  const dir = format1();
  convertToFormat2(dir);
  const before = read(dir);
  expect(() => convertToFormat2(dir)).toThrow('records storage format 2; this step converts format 1');
  expect(read(dir)).toEqual(before);
  convertToFormat3(dir);
  expect(() => convertToFormat3(dir)).toThrow('records storage format 3; this step converts format 2');
});

/** A format 2 environment: one attribute row per list index */
function format2(rows: Record<string, unknown>): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ears-format-2-'));
  dirs.push(dir);
  const root = open({ path: dir, maxDbs: 8, compression: true });
  root.openDB({ name: 'meta', encoding: 'json' }).putSync('format', 2);
  root.openDB({ name: 'entities', encoding: 'json' }).putSync('Note-1', { type: 'Note' });
  root.openDB({ name: 'relations', encoding: 'json' });
  const attrs = root.openDB({ name: 'attrs', encoding: 'json' });
  for (const [key, value] of Object.entries(rows)) {
    attrs.putSync(key.replaceAll('|', SEP), { t: Array.isArray(value) ? 'array' : value === null ? 'null' : typeof value, v: value });
  }
  root.close();
  return dir;
}

it('joins each attribute\'s rows into one, a single row its value and several rows or a role one list, and moves the trash into the engine', () => {
  const dir = format2({
    'title|Note-1|0': 'kept',
    'tags|Note-1|0': ['a', 'b'],
    'tag|Note-1|1': 'second', 'tag|Note-1|0': 'first',
    'role|Note-1|0': 'current',
    'gap|Note-1|1': 'after a hole',
    'meta|Note-1|0': { k: 1 },
    'deleted|Note-2|0': true, 'deletedAt|Note-2|0': 7,
    'deleted|Note-3|0': true, 'updatedAt|Note-3|0': 4,
    'deleted|Note-4|0': false, 'deletedAt|Note-4|0': null,
  });
  expect(convertToFormat3(dir)).toEqual({ singleValues: 4, joinedLists: 3, trashed: 2 });
  const { attrs, format } = read(dir);
  expect(format).toBe(3);
  expect(Object.fromEntries(Object.entries(attrs).map(([key, { v }]) => [key.replace(SEP, '|'), v]))).toEqual({
    'title|Note-1': 'kept',
    'tags|Note-1': ['a', 'b'],
    'tag|Note-1': ['first', 'second'],
    'role|Note-1': ['current'],
    'gap|Note-1': [null, 'after a hole'],
    'meta|Note-1': { k: 1 },
    'trashedAt|Note-2': 7,
    'trashedAt|Note-3': 4,
    'updatedAt|Note-3': 4,
  });
});

it('converts to the current format a step at a time, and leaves a current one alone', () => {
  const dir = format1();
  expect(convertToCurrentFormat(dir).map(({ to }) => to)).toEqual([2, 3, 4]);
  expect(read(dir)).toMatchObject({ attrKeys: [], relations: [], format: LMDB_FORMAT_VERSION });
  expect(convertToCurrentFormat(dir)).toEqual([]);
});

it('loads format 3 into the engine\'s keys: entities in the order they were created, relations under their ids, sequences as sequences', () => {
  const dir = format1();
  convertToFormat2(dir);
  convertToFormat3(dir);
  // A numbering row, as format 3's engine kept one: the next short code of Note under N is N-5
  const root = open({ path: dir, maxDbs: 8, compression: true });
  root.openDB({ name: 'entities', encoding: 'json' }).putSync('Sequence-Note:N', { type: 'Sequence' });
  root.openDB({ name: 'attrs', encoding: 'json' }).putSync(`next${SEP}Sequence-Note:N`, { t: 'number', v: 5 });
  // A field format 3's adapter stored with no value, which its hydration read as absent
  root.openDB({ name: 'attrs', encoding: 'json' }).putSync(`description${SEP}Note-1`, { t: 'undefined' });
  root.close();
  expect(convertToFormat4(dir)).toEqual({ entities: 2, attributes: 3, relations: 1, orphanedAttributes: 0, invalidRelations: 0, sequences: 1, emptyAttributes: 1 });
  expect(() => convertToFormat4(dir)).toThrow('records storage format 4; this step converts format 3');
  const store = openLmdbStore({ path: dir, readOnly: true });
  try {
    const engine = createEarsEngine({ kv: store.kv, isEntityType: () => true, isRelationKind: () => true, isRole: () => true });
    expect(engine.query('Note').ids()).toEqual(['Note-1', 'Note-2']);
    expect(engine.getRow('Note-1' as EARS.EntityId)).toEqual({ id: 'Note-1', entityType: 'Note', createdAt: 1, title: 'kept' });
    // A relation keeps its id, its far end missing as it was
    expect(engine.findRelations({ source: 'Note-1' as EARS.EntityId })).toEqual([{ id: 'Relation-1', kind: 'refs', source: 'Note-1', target: 'Flow-1', info: undefined }]);
    expect(engine.query.all().ids()).not.toContain('Sequence-Note:N');
  } finally {
    store.close();
  }
  // The sequence numbers on from where format 3 left it
  const writable = openLmdbStore({ path: dir });
  try {
    const engine = createEarsEngine({ kv: writable.kv, isEntityType: () => true, isRelationKind: () => true, isRole: () => true, rowRules: () => ({ required: [], shortCode: 'N' }) });
    expect(engine.create('Note').row().shortCode).toBe('N-5');
  } finally {
    writable.close();
  }
});

it('compacts an environment in place, without the pages its deletes freed, and keeps what it holds', async () => {
  const dir = path.join(fs.mkdtempSync(path.join(os.tmpdir(), 'ears-compact-')), 'ears-db');
  dirs.push(path.dirname(dir));
  const engineOn = (store: ReturnType<typeof openLmdbStore>) =>
    createEarsEngine({ kv: store.kv, isEntityType: () => true, isRelationKind: () => true, isRole: () => true });
  // Pages a conversion's old tables or any deletes leave: written, then destroyed, all but one row
  const store = openLmdbStore({ path: dir });
  const engine = engineOn(store);
  engine.transact(() => { for (let i = 0; i < 200; i++) engine.create('Note', { id: `Note-${i}` as EARS.EntityId, text: 'x'.repeat(10_000) }); });
  engine.transact(() => { for (let i = 1; i < 200; i++) engine.write(`Note-${i}` as EARS.EntityId).destroy(); });
  store.close();

  const { before, after } = await compactEnvironment(dir);
  expect(after).toBeLessThan(before / 10);
  expect(fs.statSync(path.join(dir, 'data.mdb')).size).toBe(after);
  expect(fs.readdirSync(path.dirname(dir)), 'the copy it wrote beside the environment is gone').toEqual(['ears-db']);
  const reopened = openLmdbStore({ path: dir, readOnly: true });
  try {
    expect(engineOn(reopened).query('Note').ids()).toEqual(['Note-0']);
    expect(engineOn(reopened).getAttr('Note-0' as EARS.EntityId, 'text' as EARS.AttrKind)).toHaveLength(10_000);
  } finally {
    reopened.close();
  }
});

it('refuses to compact an environment that is open, and leaves its file as it was', async () => {
  const dir = path.join(fs.mkdtempSync(path.join(os.tmpdir(), 'ears-compact-')), 'ears-db');
  dirs.push(path.dirname(dir));
  const store = openLmdbStore({ path: dir });
  try {
    const engine = createEarsEngine({ kv: store.kv, isEntityType: () => true, isRelationKind: () => true, isRole: () => true });
    engine.create('Note', { id: 'Note-0' as EARS.EntityId, text: 'kept' });
    engine.query('Note').ids();
    await store.durable();
    const contents = fs.readFileSync(path.join(dir, 'data.mdb'));

    await expect(compactEnvironment(dir)).rejects.toThrow(`(reader pids: ${process.pid}`);
    expect(fs.readFileSync(path.join(dir, 'data.mdb')).equals(contents)).toBe(true);
    expect(fs.readdirSync(path.dirname(dir)), 'nothing is left beside it').toEqual(['ears-db']);
  } finally {
    store.close();
  }
});

it('refuses a record store, whose format is its own, and leaves it as it was', () => {
  const dir = path.join(fs.mkdtempSync(path.join(os.tmpdir(), 'ears-records-')), 'records');
  dirs.push(path.dirname(dir));
  const records = openRecordStore({ path: dir, indexes: {} });
  records.put({ id: 'R-1' });
  records.close();
  expect(() => convertToCurrentFormat(dir)).toThrow('are a "records" store, not an EARS database');
  const reopened = openRecordStore({ path: dir, indexes: {} });
  expect(reopened.get('R-1')).toEqual({ id: 'R-1' });
  reopened.close();
});

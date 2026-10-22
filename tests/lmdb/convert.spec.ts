// convertToFormat2: a format 1 environment keeps one record of each fact afterwards, and loses nothing else
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { afterEach, expect, it } from 'vitest';
import { open } from 'lmdb';
import { convertToFormat2 } from '../../src/lmdb/index.ts';

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
  expect(read(dir)).toEqual({
    entities: { 'Note-1': { type: 'Note' }, 'Note-2': { type: 'Note' } },
    attrKeys: [`createdAt${SEP}Note-1${SEP}0`, `createdAt${SEP}Note-2${SEP}0`, `title${SEP}Note-1${SEP}0`],
    relations: ['Relation-1'],
    format: 2,
  });
});

it('refuses an environment that records a format other than 1, changing nothing', () => {
  const dir = format1();
  convertToFormat2(dir);
  const before = read(dir);
  expect(() => convertToFormat2(dir)).toThrow('records storage format 2; only format 1 converts');
  expect(read(dir)).toEqual(before);
});

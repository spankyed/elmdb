// @ts-check
// `npm run typecheck` checks this file against the generated types, and `npm test` runs it: the schema a database
// answers is the canonical form, which writes every absent option as `null`, and the type it is declared with admits
// it; and a write's answer and a read's, decoded, are exactly values their generated types (`Written`, `Answers`) declare
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';

/** @typedef {import('../index.js').Schema} Schema */
/** @typedef {import('../index.js').Written} Written */
/** @typedef {import('../index.js').Reads} Reads */
/** @typedef {import('../index.js').Answers} Answers */

const require = createRequire(import.meta.url);
/** @type {typeof import('../index.js')} */
const { Database } = require('../index.js');

test('the canonical form a database answers is a Schema, every absent option null', (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-'));
  /** @type {import('../index.js').Database | undefined} */
  let db;
  // Closed before its directory is removed, whether or not an assertion failed: Windows won't remove a mapped file
  t.after(() => {
    db?.close();
    rmSync(dir, { recursive: true, force: true });
  });
  /** @type {Schema} */
  const given = {
    $schema: 'https://elmdb.dev/schema/1.json',
    roles: { admin: {} },
    relations: { cites: { cardinality: 'many-to-many' }, weighs: { cardinality: 'many-to-many', weighted: true } },
    ranks: { noteRank: { types: ['Note'], relations: ['cites', 'weighs'] } },
    types: {
      Note: {
        fields: { title: { type: 'string' } },
        indexes: { text: { kind: 'text', fields: { title: {} } } },
      },
    },
  };
  /** @type {Schema} */
  const canonical = {
    $schema: 'https://elmdb.dev/schema/1.json',
    collation: 'root',
    ranks: {
      noteRank: {
        damping: 0.85,
        declaredBy: null,
        direction: 'out',
        relations: ['cites', 'weighs'],
        tolerance: 1e-5,
        types: ['Note'],
        weights: true,
      },
    },
    relations: {
      cites: {
        acyclic: false,
        cardinality: 'many-to-many',
        declaredBy: null,
        endpoints: null,
        info: { nullable: true, optional: true, type: 'any' },
        key: [],
        weighted: false,
      },
      weighs: {
        acyclic: false,
        cardinality: 'many-to-many',
        declaredBy: null,
        endpoints: null,
        info: { nullable: true, optional: true, type: 'any' },
        key: [],
        weighted: true,
      },
    },
    roles: { admin: { declaredBy: null } },
    types: {
      Note: {
        additionalFields: 'refuse',
        declaredBy: null,
        fields: { title: { nullable: false, optional: false, type: 'string' } },
        indexes: { text: { analyzer: 'standard', fields: { title: { weight: 1 } }, kind: 'text', stem: null } },
        shortCode: null,
      },
    },
  };
  db = Database.open(join(dir, 'db'), { schema: given, syncs: false });
  assert.deepEqual(db.schema, canonical);
  // What a database answers, it takes back unchanged
  assert.deepEqual(db.setSchema(canonical), []);
});

test('a write answers what its generated type declares, field for field', (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-'));
  /** @type {import('../index.js').Database | undefined} */
  let db;
  // Closed before its directory is removed: Windows won't remove a file that is mapped
  t.after(() => {
    db?.close();
    rmSync(dir, { recursive: true, force: true });
  });
  /** @type {{ Packr: new (options: object) => { pack(value: unknown): Buffer, unpack(bytes: Buffer): unknown } }} */
  const { Packr } = require('msgpackr');
  const packr = new Packr({ useRecords: false, mapsAsObjects: true });
  db = Database.open(dir, {
    syncs: false,
    schema: {
      $schema: 'https://elmdb.dev/schema/1.json',
      relations: { cites: { cardinality: 'many-to-many' } },
      types: { Note: { fields: { title: { type: 'string' } } } },
    },
  });
  const answer = packr.unpack(db.write(packr.pack({
    ops: [
      { op: 'create', type: 'Note', id: 'Note-1', fields: { title: 'one' } },
      { op: 'create', type: 'Note', id: 'Note-2', fields: { title: 'two' } },
      { op: 'link', source: 'Note-1', kind: 'cites', target: 'Note-2' },
    ],
    now: 1000,
  })));
  // The relation's id is the engine's to make: read through the generated type, held to a string naming a relation
  const linked = /** @type {Written} */ (answer).results[2];
  assert.ok(linked.kind === 'relation', `the link answered ${JSON.stringify(linked)}`);
  assert.match(linked.id, /^Relation-/);
  const relation = linked.id;
  /** @type {import('../index.js').ChangeSet['entities'][string]} */
  const note = { type: 'Note', created: true, destroyed: false, trashed: false, restored: false, fields: ['createdAt', 'title', 'updatedAt'] };
  /** @type {Written} */
  const expected = {
    results: [
      { kind: 'entity', id: 'Note-1', shortCode: null, created: true },
      { kind: 'entity', id: 'Note-2', shortCode: null, created: true },
      { kind: 'relation', id: relation, linked: true },
    ],
    changes: {
      entities: { 'Note-1': note, 'Note-2': note },
      relations: { [relation]: { kind: 'cites', source: 'Note-1', target: 'Note-2', linked: true, unlinked: false, changed: false } },
      types: ['Note'],
      kinds: ['cites'],
      sequence: db.sequence,
      touched: [],
    },
    sequence: db.sequence,
  };
  assert.deepEqual(answer, expected);
});

test('a read answers what its generated types declare, kind by kind', (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-'));
  /** @type {{ Packr: new (options: object) => { pack(value: unknown): Buffer, unpack(bytes: Buffer): unknown } }} */
  const { Packr } = require('msgpackr');
  const packr = new Packr({ useRecords: false, mapsAsObjects: true });
  const db = Database.open(dir, {
    syncs: false,
    schema: {
      $schema: 'https://elmdb.dev/schema/1.json',
      relations: {
        refs: { cardinality: 'many-to-many' },
        weighs: { cardinality: 'many-to-many', weighted: true },
      },
      types: {
        Note: {
          fields: { title: { type: 'string', optional: true }, n: { type: 'number', optional: true } },
          indexes: { text: { kind: 'text', fields: { title: {} } } },
        },
      },
    },
  });
  // Closed before its directory is removed: Windows won't remove a file that is mapped
  t.after(() => {
    db.close();
    rmSync(dir, { recursive: true, force: true });
  });
  db.write(packr.pack({
    ops: [
      { op: 'create', type: 'Note', id: 'Note-1', fields: { title: 'alpha beta', n: 1 } },
      { op: 'create', type: 'Note', id: 'Note-2', fields: { title: 'alpha', n: 2 } },
      { op: 'link', source: 'Note-1', kind: 'refs', target: 'Note-2' },
      { op: 'link', source: 'Note-1', kind: 'weighs', target: 'Note-2', weight: 2.5 },
    ],
    now: 1000,
  }));
  db.waitForIndexBuilds();
  /** @type {import('../index.js').Start} */
  const notes = { from: 'types', types: ['Note'] };
  /** @type {Reads} */
  const reads = {
    reads: [
      { read: 'query', plan: { start: notes, terminal: { answer: 'ids' } } },
      { read: 'query', plan: { start: notes, terminal: { answer: 'count' } } },
      { read: 'query', plan: { start: notes, terminal: { answer: 'first' } } },
      { read: 'query', plan: { start: notes, terminal: { answer: 'exists' } } },
      { read: 'query', plan: { start: notes, terminal: { answer: 'rows', fields: ['title'] } } },
      { read: 'field', id: 'Note-1', field: 'n' },
      { read: 'query', plan: { start: notes, terminal: { answer: 'edges', kinds: ['refs', 'weighs'] } } },
      { read: 'query', plan: { start: notes, terminal: { answer: 'degrees', kinds: ['refs'] } } },
      { read: 'query', plan: { start: notes, terminal: { answer: 'weakComponents', kinds: ['refs'], counts: true } } },
      { read: 'query', plan: { start: notes, terminal: { answer: 'groupBy', field: 'n' } } },
      { read: 'query', plan: { start: notes, terminal: { answer: 'least', field: 'n' } } },
      { read: 'query', plan: { start: notes, terminal: { answer: 'sum', field: 'n' } } },
      { read: 'row', id: 'Note-9' },
      { read: 'suggest', type: 'Note', term: 'alp' },
      { read: 'query', visits: true, plan: { start: notes, terminal: { answer: 'ids' } } },
    ],
  };
  const answers = /** @type {Answers} */ (packr.unpack(db.read(packr.pack(reads))));
  // The relations' ids are the engine's to make: read through the generated type, held to strings naming relations
  const edges = answers[6];
  assert.ok(edges.kind === 'relations', `the edges answered ${JSON.stringify(edges)}`);
  const [refs, weighs] = edges.relations.map((relation) => relation.id);
  assert.match(refs, /^Relation-/);
  assert.match(weighs, /^Relation-/);
  /** @type {Answers} */
  const expected = [
    { kind: 'ids', ids: ['Note-1', 'Note-2'] },
    { kind: 'count', n: 2 },
    { kind: 'id', id: 'Note-1' },
    { kind: 'bool', value: true },
    { kind: 'rows', rows: [{ id: 'Note-1', title: 'alpha beta' }, { id: 'Note-2', title: 'alpha' }], plainObjects: true },
    { kind: 'field', present: true, value: 1 },
    {
      kind: 'relations',
      relations: [
        { id: refs, kind: 'refs', source: 'Note-1', target: 'Note-2', info: null },
        { id: weighs, kind: 'weighs', source: 'Note-1', target: 'Note-2', info: null, weight: 2.5 },
      ],
    },
    { kind: 'degrees', degrees: [['Note-1', 1], ['Note-2', 0]] },
    { kind: 'weakComponents', weakComponents: [2] },
    { kind: 'groups', groups: [[1, ['Note-1']], [2, ['Note-2']]], plainObjects: true },
    { kind: 'least', id: 'Note-1', value: 1 },
    { kind: 'sum', sum: 3, n: 2 },
    { kind: 'row', row: null, plainObjects: true },
    { kind: 'suggestions', completions: ['alpha'], corrections: [] },
    { kind: 'visited', answer: { kind: 'ids', ids: ['Note-1', 'Note-2'] }, visited: 2 },
  ];
  assert.deepEqual(answers, expected);
});

test('an answer that may hold `__proto__` decodes exactly as Maps of the keys its types declare', (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-'));
  /** @type {{ Packr: new (options: object) => { pack(value: unknown): Buffer, unpack(bytes: Buffer): unknown } }} */
  const { Packr } = require('msgpackr');
  const objects = new Packr({ useRecords: false, mapsAsObjects: true });
  const exact = new Packr({ useRecords: false, mapsAsObjects: false });
  const db = Database.open(dir, {
    syncs: false,
    schema: {
      $schema: 'https://elmdb.dev/schema/1.json',
      types: { Note: { fields: { meta: { type: 'any', optional: true } } } },
    },
  });
  t.after(() => {
    db.close();
    rmSync(dir, { recursive: true, force: true });
  });
  // A Map, as an object literal's `__proto__` would set its prototype rather than hold the key
  db.write(objects.pack({
    ops: [{ op: 'create', type: 'Note', id: 'Note-1', fields: { meta: new Map([['__proto__', 1]]) } }],
    now: 1000,
  }));
  /** @type {Reads} */
  const reads = { reads: [{ read: 'query', plan: { start: { from: 'types', types: ['Note'] }, terminal: { answer: 'rows', fields: ['meta'] } } }] };
  const bytes = db.read(objects.pack(reads));
  const [answer] = /** @type {Answers} */ (objects.unpack(bytes));
  assert.ok(answer.kind === 'rows' && answer.plainObjects === false, JSON.stringify(answer));
  // Decoded exactly, each map the types declare an object is a Map of the same keys, and the stored key is kept
  const [held] = /** @type {Map<string, unknown>[]} */ (exact.unpack(bytes));
  assert.deepEqual([...held.keys()], ['kind', 'rows', 'plainObjects']);
  const [row] = /** @type {Map<string, unknown>[]} */ (held.get('rows'));
  assert.deepEqual([...row.keys()], ['id', 'meta']);
  assert.deepEqual(row.get('meta'), new Map([['__proto__', 1]]));
});

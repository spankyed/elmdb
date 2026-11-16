// @ts-check
// `npm run typecheck` checks this file against the generated types, and `npm test` runs it: the schema a database
// answers is the canonical form, which writes every absent option as `null`, and the type it is declared with admits
// it; and a write's answer, decoded, is exactly a value its generated type (`Written`) declares
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';

/** @typedef {import('../index.js').Schema} Schema */
/** @typedef {import('../index.js').Written} Written */

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

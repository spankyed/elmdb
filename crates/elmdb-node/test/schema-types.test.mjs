// @ts-check
// `npm run typecheck` checks this file against the generated types, and `npm test` runs it: the schema a database
// answers is the canonical form, which writes every absent option as `null`, and the type it is declared with admits it
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';

/** @typedef {import('../index.js').Schema} Schema */

const require = createRequire(import.meta.url);
/** @type {typeof import('../index.js')} */
const { Database } = require('../index.js');

test('the canonical form a database answers is a Schema, every absent option null', (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
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
        tolerance: 1e-6,
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
        unique: false,
        weighted: false,
      },
      weighs: {
        acyclic: false,
        cardinality: 'many-to-many',
        declaredBy: null,
        endpoints: null,
        info: { nullable: true, optional: true, type: 'any' },
        unique: false,
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
  const db = Database.open(join(dir, 'db'), { schema: given, syncs: false });
  assert.deepEqual(db.schema, canonical);
  // What a database answers, it takes back unchanged
  assert.deepEqual(db.setSchema(canonical), []);
  db.close();
});

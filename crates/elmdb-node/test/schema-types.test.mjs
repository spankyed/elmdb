// @ts-check
// Verifies generated v2 protocol declarations and JSON Schema (`protocol/schema-2.json`).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

/** @type {import('../protocol/Schema.js').Schema} */
const validSchema = {
  $schema: 'https://elmdb.dev/schema/2.json',
  types: {
    Note: {
      fields: {
        title: { type: 'string' },
        body: { type: 'string', optional: true },
        priority: { type: 'integer', optional: true, nullable: true },
        parent: { type: 'ref', to: ['Note'], onDelete: 'setNull', optional: true, nullable: true },
      },
      indexes: {
        byPriority: { kind: 'value', fields: ['priority'] },
        byTitle: { kind: 'order', field: 'title', locale: 'en' },
        search: { kind: 'text', fields: [['title', 3], ['body', 1]] },
        fuzzy: { kind: 'trigram', fields: ['title'] },
        vec: { kind: 'vector', fields: ['body'], dims: 4, quantization: 'sq8' },
        rank: { kind: 'rank', relations: ['links'] },
      },
    },
  },
  relations: {
    links: {
      cardinality: 'manyToMany',
      endpoints: [{ source: 'Note', target: 'Note' }],
    },
  },
};

test('protocol/schema-2.json declares the canonical v2 $id and draft-2020-12 dialect', () => {
  const path = fileURLToPath(new URL('../protocol/schema-2.json', import.meta.url));
  const json = JSON.parse(readFileSync(path, 'utf8'));
  assert.equal(json.$id, 'https://elmdb.dev/schema/2.json');
  assert.equal(json.$schema, 'https://json-schema.org/draft/2020-12/schema');
  assert.equal(validSchema.$schema, json.$id);
});

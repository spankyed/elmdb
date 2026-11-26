// @ts-check
// Verifies elmdb/client zero-dependency binary wire batch encoder (`elmdb-wire` EWB2).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
/** @type {typeof import('../index.js')} */
const { Database } = require('../index.js');
/** @type {typeof import('../client.js')} */
const { BatchSlot, WireBatchBuilder, buildWireBatch } = require('../client.js');

/** @type {import('../protocol/Schema.js').Schema} */
const schema = {
  types: {
    Item: {
      fields: {
        title: { type: 'string' },
        count: { type: 'integer', optional: true },
        score: { type: 'number', optional: true },
        when: { type: 'timestamp', optional: true },
        raw: { type: 'bytes', optional: true },
        meta: { type: 'object', optional: true },
        tags: { type: 'list', of: { type: 'string' }, optional: true },
      },
      indexes: {
        byTitle: { kind: 'value', fields: ['title'] },
      },
    },
  },
  relations: {
    links: {
      endpoints: [{ source: 'Item', target: 'Item' }],
      weighted: true,
    },
  },
};

test('WireBatchBuilder encodes all value types, BatchSlot refs, and executes in one crossing', (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-client-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const db = Database.open(dir, { schema, durability: 'unsafe', buildPolicy: 'eager' });
  t.after(() => db.close());

  const frame = buildWireBatch((b) => {
    const a = b.create('Item', {
      title: 'Alpha',
      count: 42,
      score: 3.14,
      when: new Date(1700000000000),
      raw: new Uint8Array([1, 2, 3, 255]),
      // Unsorted keys test UTF-8 byte sorting parity with Rust
      meta: { z: 1, a: 'ok', nested: { b: true, a: null } },
      tags: ['rust', 'lmdb'],
    }, 'item-a');
    assert.ok(a instanceof BatchSlot);
    const c = b.create('Item', { title: 'Beta', count: 7 }, 'item-b');
    b.link(a, 'links', c, { weight: 2.5, info: { note: 'intra-batch' } });
    b.patch(c, { count: 9 });
  });

  assert.equal(frame[0], 0x45); // 'E'
  assert.equal(frame[1], 0x57); // 'W'
  assert.equal(frame[2], 0x42); // 'B'
  assert.equal(frame[3], 0x32); // '2'

  const res = db.executeWireBatch(frame);
  assert.deepEqual(res.createdIds, ['item-a', 'item-b']);

  const rowA = db.get('item-a');
  assert.ok(rowA);
  assert.equal(rowA.fields.title, 'Alpha');
  assert.equal(rowA.fields.count, 42);

  const rowB = db.get('item-b');
  assert.ok(rowB);
  assert.equal(rowB.fields.count, 9);

  const edges = db.getEdges('item-a', 'links', 'out');
  assert.equal(edges.length, 1);
  assert.equal(edges[0].targetId, 'item-b');
  assert.equal(edges[0].weight, 2.5);
});

test('buildWireBatch rejects async/thenable callbacks and non-finite numbers synchronously', () => {
  assert.throws(
    () => buildWireBatch(/** @type {any} */ (async () => {})),
    /synchronous/,
  );
  const b = new WireBatchBuilder();
  b.create('Item', { title: 'bad', score: Number.NaN });
  assert.throws(() => b.encode(), /Non-finite/);
});

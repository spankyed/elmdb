// @ts-check
// Verifies elmdb/client zero-dependency binary wire batch encoder (`elmdb-wire` EWB2).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

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
  const db = Database.open(dir, { schema, durability: 'unsafe', buildPolicy: 'eager' });
  t.after(() => {
    try { db.close(); } finally { rmSync(dir, { recursive: true, force: true }); }
  });

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

test('buildWireBatch rejects async/thenable callbacks, __proto__, non-plain objects, BatchSlot values, and bad flags', () => {
  assert.throws(
    () => buildWireBatch(/** @type {any} */ (async () => {})),
    /synchronous/,
  );
  const b = new WireBatchBuilder();
  b.create('Item', { title: 'bad', score: Number.NaN });
  assert.throws(() => b.encode(), /Non-finite/);

  // __proto__ rejected in both top-level fields and nested objects
  const protoField = Object.create(null);
  protoField['__proto__'] = { x: 1 };
  assert.throws(() => buildWireBatch((x) => { x.create('Item', protoField); }), /__proto__/);
  const nestedProto = Object.create(null);
  nestedProto['__proto__'] = 1;
  assert.throws(() => buildWireBatch((x) => { x.create('Item', { title: 'ok', meta: nestedProto }); }), /__proto__/);

  // BatchSlot rejected inside field value
  assert.throws(() => buildWireBatch((x) => {
    const s = x.create('Item', { title: 'a' });
    x.create('Item', { title: 'b', meta: s });
  }), /BatchSlot/);

  // Non-plain objects (Map, Float32Array) and prototype-spoofed Uint8Array/Date rejected
  assert.throws(() => buildWireBatch((x) => { x.create('Item', { title: 'm', meta: new Map() }); }), /Unsupported/);
  assert.throws(() => buildWireBatch((x) => { x.create('Item', { title: 'f', raw: /** @type {any} */ (new Float32Array(2)) }); }), /Unsupported/);
  assert.throws(() => buildWireBatch((x) => { x.create('Item', { title: 's1', raw: Object.create(Uint8Array.prototype) }); }), TypeError);
  assert.throws(() => buildWireBatch((x) => {
    const spoofedView = Object.setPrototypeOf(new Int32Array([1, 2]), Uint8Array.prototype);
    x.create('Item', { title: 's2', raw: spoofedView });
  }), TypeError);
  assert.throws(() => buildWireBatch((x) => { x.create('Item', { title: 's3', ts: Object.create(Date.prototype) }); }), TypeError);

  // Own spoofed `byteLength` on a real Uint8Array is ignored via intrinsic TypedArray getters
  const realWithFakeLen = new Uint8Array([0xaa, 0xbb]);
  Object.defineProperty(realWithFakeLen, 'byteLength', { value: 99999 });
  const encodedReal = buildWireBatch((x) => { x.create('Item', { title: 'ok', raw: realWithFakeLen }); });
  assert.ok(encodedReal.byteLength < 100);

  // Cross-realm Date and Uint8Array encode cleanly via intrinsic brand checks
  const crossRealmDate = vm.runInNewContext('new Date(1700000000000)');
  const crossRealmBytes = vm.runInNewContext('new Uint8Array([1, 2, 3])');
  const encodedCross = buildWireBatch((x) => { x.create('Item', { title: 'xrealm', ts: crossRealmDate, raw: crossRealmBytes }); });
  assert.ok(encodedCross.byteLength > 0);

  // Lone UTF-16 surrogates rejected in field names and nested object keys (while valid emoji surrogate pairs succeed)
  assert.throws(() => buildWireBatch((x) => { x.create('Item', { ['bad\uD800key']: 1 }); }), /surrogate/);
  assert.throws(() => buildWireBatch((x) => { x.create('Item', { title: 'ok', meta: { ['\uD800']: 1, ['\uD801']: 2 } }); }), /surrogate/);
  assert.throws(() => buildWireBatch((x) => { x.create('Item', { title: 'ok', meta: { ['\uDC00']: 1 } }); }), /surrogate/);
  const validEmoji = buildWireBatch((x) => { x.create('Item', { title: 'ok', meta: { ['😀']: 1 } }); });
  assert.ok(validEmoji.byteLength > 0);

  // Invalid BatchSlot index and invalid returnFlags rejected
  assert.throws(() => new BatchSlot(-1), RangeError);
  const b2 = new WireBatchBuilder();
  assert.throws(() => b2.encode(0xff), RangeError);
});

test('examples/diagnostics.mjs runs end-to-end against real Database & WireBatchBuilder without drifting (F5)', (t) => {
  const testDir = dirname(fileURLToPath(import.meta.url));
  const examplePath = join(testDir, '..', 'examples', 'diagnostics.mjs');
  const indexPath = join(testDir, '..', 'index.js');
  const shimDir = mkdtempSync(join(tmpdir(), 'elmdb-diag-shim-'));
  t.after(() => {
    rmSync(shimDir, { recursive: true, force: true });
  });
  const shimPath = join(shimDir, 'diag-binding.cjs');
  writeFileSync(
    shimPath,
    `const { Database } = require(${JSON.stringify(indexPath)});\n`
      + 'module.exports = { Database, collectDiagnostics() {}, takeDiagnostics() { return { spans: [], dropped: 0 }; } };\n',
  );
  const out = execFileSync(process.execPath, [examplePath, shimPath], { encoding: 'utf8' });
  assert.match(out, /0 spans, 0 dropped/);
});

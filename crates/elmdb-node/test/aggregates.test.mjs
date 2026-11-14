// least, greatest and sum through the binding: each is a plan's terminal, answered as msgpack like any other answer.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import { Packr } from 'msgpackr';

const { Database } = createRequire(import.meta.url)('../index.js');
const packr = new Packr({ useRecords: false, moreTypes: true, mapsAsObjects: true });
const schema = {
  $schema: 'https://elmdb.dev/schema/1.json',
  types: { Item: { fields: { price: { type: 'number', optional: true }, qty: { type: 'integer', optional: true }, name: { type: 'string', optional: true } } } },
};

test('least, greatest and sum answer what the set of a plan holds, through the binding', (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-aggregates-'));
  const db = Database.open(dir, { schema, syncs: false });
  // One hook, closing first: Windows won't remove the files of a database that is open
  t.after(() => {
    db.close();
    rmSync(dir, { recursive: true, force: true });
  });
  const prices = [5, 2, 9, undefined, 2, 7.5];
  db.writeSync(packr.pack({
    ops: prices.map((price, i) => ({ op: 'create', type: 'Item', id: `Item-${i}`, fields: { ...(price === undefined ? {} : { price }), qty: 1, name: `item ${i}` } })),
    now: 1000,
  }));
  const answer = (terminal) => packr.unpack(db.readSync(packr.pack({
    reads: [{ read: 'query', plan: { start: { from: 'types', types: ['Item'] }, terminal } }],
  })))[0];
  assert.deepEqual(answer({ answer: 'least', field: 'price' }), { kind: 'least', id: 'Item-1', value: 2 });
  assert.deepEqual(answer({ answer: 'greatest', field: 'price' }), { kind: 'greatest', id: 'Item-2', value: 9 });
  assert.deepEqual(answer({ answer: 'sum', field: 'price' }), { kind: 'sum', sum: 25.5, n: 5 });
  assert.deepEqual(answer({ answer: 'sum', field: 'qty' }), { kind: 'sum', sum: 6, n: 6 });
  assert.throws(() => answer({ answer: 'sum', field: 'name' }), { code: 'VALUE_INVALID' });
});

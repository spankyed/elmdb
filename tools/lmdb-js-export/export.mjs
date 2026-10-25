#!/usr/bin/env node
// Exports an apack format 4 database into a dump `elmdb upgrade` reads, or an apack record store (the run history)
// into one apack's run-history import reads:
//
//   node export.mjs <ears-db directory> <dump file>
//   node export.mjs --records <record store directory> <dump file>
//
// lmdb-js writes LMDB files in its own data version (2), which stock LMDB (version 1, the one elmdb runs on) refuses
// to open, so a format 4 database can't be read by elmdb directly. This copies the `kv` database's entries, byte for
// byte and in key order, into a file of its own; the format 4 key layout is what `elmdb upgrade` understands, and only
// the container changes. The source is opened read-only.
//
// The dump: the 8 bytes `ELMDBF4\0`, a u32 dump version (1), then for each entry a u32 key length, the key, a u32 value
// length and the value, then a u32 0xFFFFFFFF and a u64 entry count. Integers are little-endian.
//
// A record store's dump is JSON lines: a header, `{"dump":"apack-records","version":1,"format":1}` with the store's
// format, then each record as the store held it, in id order, which is the order the store made them.
import { open } from 'lmdb';
import { closeSync, existsSync, openSync, writeSync } from 'node:fs';
import { join } from 'node:path';

const args = process.argv.slice(2);
const records = args[0] === '--records';
const [source, target] = records ? args.slice(1) : args;
if (!source || !target) {
  console.error('usage: node export.mjs <ears-db directory> <dump file>\n       node export.mjs --records <record store directory> <dump file>');
  process.exit(2);
}
if (!existsSync(join(source, 'data.mdb'))) {
  console.error(`No LMDB database at ${source}`);
  process.exit(1);
}
if (existsSync(target)) {
  console.error(`${target} exists; the export writes a new file`);
  process.exit(1);
}

const root = open({ path: source, readOnly: true, maxDbs: 8 });
const meta = root.openDB({ name: 'meta', encoding: 'json' });
if (records) {
  const format = meta.get('format');
  const kind = meta.get('kind');
  if (format !== 1 || kind !== 'records') {
    console.error(`${source} records format ${JSON.stringify(format)} of kind ${JSON.stringify(kind)}; the export reads a record store in format 1`);
    root.close();
    process.exit(1);
  }
  const db = root.openDB({ name: 'records', encoding: 'json' });
  const fd = openSync(target, 'wx');
  writeSync(fd, `${JSON.stringify({ dump: 'apack-records', version: 1, format })}\n`);
  let count = 0;
  let lines = [];
  for (const { value } of db.getRange()) {
    lines.push(JSON.stringify(value));
    count++;
    if (lines.length >= 1000) { writeSync(fd, `${lines.join('\n')}\n`); lines = []; }
  }
  if (lines.length) writeSync(fd, `${lines.join('\n')}\n`);
  closeSync(fd);
  root.close();
  console.log(JSON.stringify({ source, target, records: count }));
  process.exit(0);
}
const format = meta.get('format');
const kind = meta.get('kind') ?? 'ears';
if (format !== 4 || kind !== 'ears') {
  console.error(`${source} records format ${JSON.stringify(format)} of kind ${JSON.stringify(kind)}; the export reads an EARS database in format 4 (convert it with apack's scripts/ears-convert.ts first)`);
  root.close();
  process.exit(1);
}
const kv = root.openDB({ name: 'kv', keyEncoding: 'binary', encoding: 'binary', compression: false });

const fd = openSync(target, 'wx');
const u32 = (n) => { const b = Buffer.alloc(4); b.writeUInt32LE(n); return b; };
let pending = [];
let pendingBytes = 0;
const flush = () => { if (pending.length) { writeSync(fd, Buffer.concat(pending)); pending = []; pendingBytes = 0; } };
const push = (b) => { pending.push(b); pendingBytes += b.length; if (pendingBytes > 1 << 20) flush(); };

push(Buffer.from('ELMDBF4\0', 'latin1'));
push(u32(1));
let count = 0n;
for (const { key, value } of kv.getRange()) {
  push(u32(key.length)); push(Buffer.from(key));
  push(u32(value.length)); push(Buffer.from(value));
  count++;
}
push(u32(0xffffffff));
const tail = Buffer.alloc(8); tail.writeBigUInt64LE(count); push(tail);
flush();
closeSync(fd);
root.close();
console.log(JSON.stringify({ source, target, entries: Number(count) }));

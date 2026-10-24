// The crash harness's child (tests/lmdb/crash.integration.spec.ts): runs a seeded workload against an LMDB store and acknowledges each write on stdout once
// it has returned, until it is killed. Run as: node --import tsx crash-child.mts <dir> <seed> <length>
import * as fs from 'node:fs';
import fc from 'fast-check';
import { openLmdbStore } from '../../src/lmdb/index.ts';
import { newEngine } from './engines.ts';
import { applyTo } from './model.ts';
import { crashWorkload } from './workload.ts';

const [dir, seed, length] = process.argv.slice(2);
const [ops] = fc.sample(crashWorkload(Number(length)), { seed: Number(seed), numRuns: 1 });

const store = openLmdbStore({ path: dir });
const engine = newEngine(store.kv);
for (let i = 0; i < ops.length; i++) {
  applyTo(engine, ops[i], () => {});
  fs.writeSync(1, `${i + 1}\n`);
}
fs.writeSync(1, 'done\n');
setInterval(() => {}, 1000);

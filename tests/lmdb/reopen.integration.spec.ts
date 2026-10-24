// The correctness harness, the files against the engine: random workloads (tests/_harness/) run on an LMDB store,
// which, opened again, holds exactly what the engine held.
//
// @slow: each run opens and reopens a real environment in a temp dir of its own
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import fc from 'fast-check';
import { afterAll, describe, expect, it } from 'vitest';
import { openLmdbStore } from '../../src/lmdb/index.ts';
import { applyTo, stateOf } from '../_harness/model.ts';
import { newEngine } from '../_harness/engines.ts';
import { workload } from '../_harness/workload.ts';

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'ears-harness-'));
afterAll(() => fs.rmSync(root, { recursive: true, force: true }));

describe('the files against the engine', () => {
  let run = 0;
  it('open again to exactly what the engine held', async () => {
    await fc.assert(fc.asyncProperty(workload(30), async (ops) => {
      const dir = path.join(root, `run-${run++}`, 'ears-db');
      const store = openLmdbStore({ path: dir });
      const engine = newEngine(store.kv);
      // The store refuses nothing here: a full disk is the model-based spec's, over a backend that can refuse
      for (const op of ops) applyTo(engine, op, () => {});
      const held = stateOf(engine);
      store.close();
      const again = openLmdbStore({ path: dir });
      expect(stateOf(newEngine(again.kv))).toEqual(held);
      again.close();
    }), { numRuns: 60 });
  });
});

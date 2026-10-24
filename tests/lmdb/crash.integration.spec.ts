// The correctness harness, crash injection (tests/_harness/): a child runs a seeded workload against an LMDB store,
// acknowledging each write once it has returned, and is killed (SIGKILL) at a random moment. Reopened, the files hold exactly what the
// model holds after the writes acknowledged — or after one more, the write in flight when the kill landed.
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { spawn } from 'node:child_process';
import fc from 'fast-check';
import { afterAll, expect, it } from 'vitest';
import { openLmdbStore } from '../../src/lmdb/index.ts';
import { newEngine } from '../_harness/engines.ts';
import { Model, stateOf, type State } from '../_harness/model.ts';
import { crashWorkload } from '../_harness/workload.ts';

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'ears-crash-'));
afterAll(() => fs.rmSync(root, { recursive: true, force: true }));
const CHILD = path.join(import.meta.dirname, '..', '_harness', 'crash-child.mts');
const LENGTH = 400;

/** Runs the child until it has acknowledged `killAt` writes (or finished), kills it, and says how many it acknowledged */
function runAndKill(dir: string, seed: number, killAt: number): Promise<number> {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, ['--import', 'tsx', CHILD, dir, String(seed), String(LENGTH)], { stdio: ['ignore', 'pipe', 'pipe'] });
    let acked = 0;
    let buffered = '';
    let errors = '';
    child.stderr.on('data', (chunk) => { errors += chunk; });
    child.stdout.on('data', (chunk) => {
      buffered += chunk;
      const lines = buffered.split('\n');
      buffered = lines.pop()!;
      for (const line of lines) {
        if (line === 'done') child.kill('SIGKILL');
        else acked = Number(line);
        if (acked >= killAt) child.kill('SIGKILL');
      }
    });
    // `close`, not `exit`: the child's stdout may still be draining at `exit`, and an acknowledgement read late would
    // be counted as never made
    child.on('close', (code, signal) => {
      if (signal === 'SIGKILL') resolve(acked);
      else reject(new Error(`the child exited ${code} before it was killed: ${errors}`));
    });
  });
}

it('reopens a killed process to exactly the writes that returned, and at most the one in flight', async () => {
  let run = 0;
  await fc.assert(fc.asyncProperty(fc.integer({ min: 1, max: 2 ** 30 }), fc.integer({ min: 1, max: LENGTH }), async (seed, killAt) => {
    const dir = path.join(root, `run-${run++}`, 'ears-db');
    const acked = await runAndKill(dir, seed, killAt);

    // The model's state after each prefix of the same workload
    const [ops] = fc.sample(crashWorkload(LENGTH), { seed, numRuns: 1 });
    const model = new Model();
    const after: State[] = [model.state()];
    for (const op of ops) {
      model.apply(op);
      after.push(model.state());
    }

    const store = openLmdbStore({ path: dir });
    const stored = stateOf(newEngine(store.kv));
    store.close();
    const candidates = [after[acked], after[Math.min(acked + 1, ops.length)]];
    expect(candidates, `seed ${seed}, killed after ${acked} acknowledged writes`).toContainEqual(stored);
  }), { numRuns: 6 });
});

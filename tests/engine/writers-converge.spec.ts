// The invariant the database's change log exists for: with another process writing in any order, killed part way or
// not, a watch here ends up showing what its read answers afresh, once this engine has heard. A random history of real
// commits from a real second process, against every read the model-based harness watches.
import { spawn, type ChildProcess } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import fc from 'fast-check';
import { expect, it } from 'vitest';
import { createEarsEngine, type EARS, type EarsEngine } from '../../src/index.ts';
import { openDatabase } from '../../src/lmdb/index.ts';
import { INDEXED, KINDS, ROLES, TYPES, watchedReads, type Op } from '../_harness/model.ts';

const IDS = ['Task-1', 'Task-2', 'Note-1', 'Note-2'];

type RawOp = Record<string, unknown>;
type Step =
  | { step: 'here'; op: RawOp }
  | { step: 'there'; op: RawOp; die: boolean }
  | { step: 'tell' };

const anId = fc.constantFrom(...IDS);
const aRawOp: fc.Arbitrary<RawOp> = fc.oneof(
  anId.map((id) => ({ op: 'create', type: id.split('-')[0], id })),
  fc.tuple(anId, fc.integer({ min: 0, max: 9 })).map(([id, value]) => ({ op: 'set', id, fields: { [INDEXED]: value } })),
  anId.map((id) => ({ op: 'trash', id })),
  anId.map((id) => ({ op: 'destroy', id })),
  fc.tuple(anId, anId).map(([source, target]) => ({ op: 'link', source, target, kind: KINDS[0] })),
  fc.tuple(anId, anId).map(([source, target]) => ({ op: 'unlink', source, target, kind: KINDS[0] })),
);
const aStep: fc.Arbitrary<Step> = fc.oneof(
  aRawOp.map((op) => ({ step: 'here' as const, op })),
  { arbitrary: fc.tuple(aRawOp, fc.boolean()).map(([op, die]) => ({ step: 'there' as const, op, die })), weight: 3 },
  fc.constant({ step: 'tell' as const }),
);

const requireFromEars = createRequire(createRequire(import.meta.url).resolve('@apackdev/ears/package.json'));

/** Another process holding the database open: it applies each op it's sent, one commit each, and answers */
function anotherProcess(path: string) {
  const child: ChildProcess = spawn(process.execPath, ['-e', `
    const { Database } = require(${JSON.stringify(requireFromEars.resolve('elmdb'))});
    const { Packr } = require(${JSON.stringify(requireFromEars.resolve('msgpackr'))});
    const packr = new Packr({ useRecords: false, moreTypes: true, mapsAsObjects: true });
    const database = Database.open(process.argv[1]);
    let held = '';
    process.stdin.setEncoding('utf8').on('data', (chunk) => {
      held += chunk;
      for (let at; (at = held.indexOf('\\n')) >= 0; held = held.slice(at + 1)) {
        const { op, die } = JSON.parse(held.slice(0, at));
        try { database.write(packr.pack({ ops: [op], now: 1 })); } catch {}
        if (die) process.kill(process.pid, 'SIGKILL');
        process.stdout.write('done\\n');
      }
    });
    process.stdout.write('ready\\n');
  `, path], { stdio: ['pipe', 'pipe', 'inherit'] });
  const heard: Array<() => void> = [];
  child.stdout!.setEncoding('utf8').on('data', (chunk: string) => {
    for (const _ of chunk.trim().split('\n')) heard.shift()?.();
  });
  const answered = () => new Promise<void>((resolve) => { heard.push(resolve); });
  const gone = new Promise<void>((resolve) => { child.once('close', () => resolve()); });
  return {
    ready: answered(),
    async send(op: RawOp, die: boolean) {
      const reply = die ? gone : answered();
      child.stdin!.write(`${JSON.stringify({ op, die })}\n`);
      await reply;
    },
    async stop() {
      child.kill('SIGKILL');
      await gone;
    },
    gone: () => child.exitCode !== null || child.signalCode !== null,
  };
}

/** One of the ops, as this process's engine makes it */
function writeHere(engine: EarsEngine, op: RawOp): void {
  const id = op.id as EARS.EntityId;
  switch (op.op) {
    case 'create': engine.create(op.type as string, { id }); break;
    case 'set': engine.write(id).set(INDEXED, (op.fields as Record<string, unknown>)[INDEXED]); break;
    case 'trash': engine.write(id).trash(); break;
    case 'destroy': engine.write(id).destroy(); break;
    case 'link': engine.write(op.source as EARS.EntityId).link(op.kind as EARS.RelKind, op.target as EARS.EntityId); break;
    case 'unlink': engine.write(op.source as EARS.EntityId).unlink(op.kind as EARS.RelKind, op.target as EARS.EntityId); break;
  }
}

it('leaves every watch showing what its read answers afresh, whatever another process did and however it ended', async () => {
  await fc.assert(fc.asyncProperty(fc.array(aStep, { minLength: 5, maxLength: 25 }), async (steps) => {
    const directory = mkdtempSync(join(tmpdir(), 'ears-converge-'));
    const database = openDatabase({ path: directory, syncs: false, schema: {
      types: Object.fromEntries(TYPES.map((type) => [type, { required: [], indexes: [INDEXED] }])),
      relations: Object.fromEntries(KINDS.map((kind) => [kind, {}])), roles: [...ROLES],
    } });
    const engine = createEarsEngine({ database });
    const stops: Array<() => void> = [];
    let there = anotherProcess(directory);
    try {
      await there.ready;
      const ids = IDS.map((id) => ({ op: 'create', id })) as unknown as Op[];
      const latest = new Map<string, unknown>();
      const reads = watchedReads(engine, ids);
      for (const [name, read] of Object.entries(reads)) {
        const watching = engine.watch(read, {
          next: (value) => { latest.set(name, value); },
          error: (error) => { latest.set(name, { threw: String(error) }); },
        });
        stops.push(() => watching.stop());
      }
      for (const step of steps) {
        if (step.step === 'here') {
          // Through the engine, as this process writes: a refused op is no commit
          try { writeHere(engine, step.op); } catch { /* the same op refused there is no commit either */ }
        } else if (step.step === 'tell') {
          engine.admin.changedElsewhere();
        } else {
          await there.send(step.op, step.die);
          if (there.gone()) {
            there = anotherProcess(directory);
            await there.ready;
          }
        }
      }
      engine.admin.changedElsewhere();
      await engine.admin.watchesSettled();
      for (const [name, read] of Object.entries(reads)) {
        let fresh: unknown;
        try { fresh = read(); } catch (error) { fresh = { threw: String(error) }; }
        expect(latest.get(name), name).toEqual(fresh);
      }
    } finally {
      for (const stop of stops) stop();
      await there.stop();
      database.close();
      rmSync(directory, { recursive: true, force: true });
    }
  }), { numRuns: 25 });
});

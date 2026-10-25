// Closing a store while lmdb-js syncs it off the main thread: in a process of its own, since closing the environment
// under a running sync ends the process with SIGSEGV, which in a test worker would take the worker with it
import { spawn } from 'node:child_process';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { afterAll, expect, it } from 'vitest';

const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ears-lmdb-close-'));
afterAll(() => fs.rmSync(dir, { recursive: true, force: true }));

it('closes while a sync runs off the main thread, the files closing once it has finished', async () => {
  const child = path.join(dir, 'closer.mts');
  const source = (file: string) => pathToFileURL(fileURLToPath(new URL(`../../src/${file}`, import.meta.url))).href;
  fs.writeFileSync(child, [
    `import * as path from 'node:path';`,
    `import { createKvEngine } from ${JSON.stringify(source('kv/engine.ts'))};`,
    `import { openLmdbStore } from ${JSON.stringify(source('lmdb/index.ts'))};`,
    `for (let i = 0; i < 300; i++) {`,
    `  const store = openLmdbStore({ path: path.join(process.argv[2], String(i)) });`,
    `  createKvEngine({ kv: store.kv, isEntityType: () => true, isRelationKind: () => true, isRole: () => true }).create('Note', { title: 'x'.repeat(5000) });`,
    `  void store.durable();`,
    `  store.close();`,
    `}`,
  ].join('\n'));
  const closer = spawn(process.execPath, ['--import', 'tsx', child, dir], { stdio: ['ignore', 'ignore', 'pipe'] });
  let errors = '';
  closer.stderr.on('data', (chunk) => { errors += String(chunk); });
  const [code, signal] = await new Promise<[number | null, NodeJS.Signals | null]>((resolve) => closer.on('exit', (...ended) => resolve(ended)));
  expect({ code, signal, errors }).toEqual({ code: 0, signal: null, errors: '' });
});

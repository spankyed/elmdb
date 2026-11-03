import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, '../../..');

test('the README\'s example runs and prints what it shows', (t) => {
  // A Windows checkout holds the README with CRLF line endings
  const readme = readFileSync(join(root, 'README.md'), 'utf8').replaceAll('\r\n', '\n');
  const example = /```js\n([\s\S]*?)```/.exec(readme)?.[1];
  const shown = /```text\n([\s\S]*?)```/.exec(readme)?.[1];
  assert.ok(example && shown, 'the README holds a ```js example and the ```text it prints');

  const dir = mkdtempSync(join(tmpdir(), 'elmdb-readme-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const source = example
    .replace("from 'elmdb'", `from ${JSON.stringify(join(here, '../index.js'))}`)
    .replace("'./notes'", JSON.stringify(join(dir, 'notes')));
  const run = spawnSync(process.execPath, ['--input-type=module'], { input: source, cwd: root, encoding: 'utf8' });
  assert.equal(run.status, 0, run.stderr);
  assert.equal(run.stdout, shown);
});

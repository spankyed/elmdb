// Records the actual loaded binding, then verifies later runtimes use those exact bytes without a rebuild.
const assert = require('node:assert/strict');
const { createHash } = require('node:crypto');
const { readFileSync, writeFileSync } = require('node:fs');
const { resolve, relative } = require('node:path');

const root = resolve(__dirname, '..');
const mode = process.argv[2];
const expectedRuntime = process.argv[3];
assert.equal(process.argv.length, 4, 'usage: runtime-binding.cjs record|verify node-floor|node24|electron');
if (expectedRuntime === 'node-floor') {
  const declared = require('../crates/elmdb-node/package.json').engines.node;
  assert.match(declared, /^>=\d+\.\d+\.\d+$/, 'update the runtime-floor check for a different engines contract');
  assert.equal(process.versions.node, declared.slice(2), 'CI must exercise the exact supported minimum from engines.node');
  assert.equal(process.versions.electron, undefined);
} else if (expectedRuntime === 'node24') {
  assert.equal(process.versions.node.split('.')[0], '24');
  assert.equal(process.versions.electron, undefined);
} else if (expectedRuntime === 'electron') {
  assert.equal(process.versions.electron, '37.2.4', 'the Electron executable must run this check');
} else {
  assert.fail('unknown runtime contract');
}
assert.ok(mode === 'record' || mode === 'verify', 'usage: runtime-binding.cjs record|verify');
assert.ok(process.env.RUNNER_TEMP, 'CI supplies RUNNER_TEMP for the per-job binding identity');
const identityPath = resolve(process.env.RUNNER_TEMP, 'elmdb-runtime-binding.json');
// Load through the shipped loader, not by choosing an arbitrary binary that happens to be present.
const { Database } = require('../crates/elmdb-node/index.js');
assert.equal(typeof Database.open, 'function');
const binaries = Object.keys(require.cache).filter((path) => path.endsWith('.node') && path.startsWith(resolve(root, 'crates/elmdb-node') + require('node:path').sep));
assert.equal(binaries.length, 1, 'one local elmdb binding must load');
const identity = { path: relative(root, binaries[0]), sha256: createHash('sha256').update(readFileSync(binaries[0])).digest('hex') };
if (mode === 'record') writeFileSync(identityPath, JSON.stringify(identity));
else assert.deepEqual(identity, JSON.parse(readFileSync(identityPath, 'utf8')), 'runtime must load the original binding without rebuilding');
console.log(JSON.stringify({ runtime: process.versions, binding: identity }));

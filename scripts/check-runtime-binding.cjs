// A wrong recorded hash and a wrong runtime must each be refused by the real verifier.
const assert = require('node:assert/strict');
const { readFileSync, writeFileSync } = require('node:fs');
const { resolve } = require('node:path');
const { spawnSync } = require('node:child_process');
assert.ok(process.env.RUNNER_TEMP);
const identityPath = resolve(process.env.RUNNER_TEMP, 'elmdb-runtime-binding.json');
const original = readFileSync(identityPath, 'utf8');
const verify = (runtime) => spawnSync(process.execPath, [resolve(__dirname, 'runtime-binding.cjs'), 'verify', runtime], { encoding: 'utf8', timeout: 20000 });
assert.equal(verify('node-floor').status, 0, 'unmodified runtime and binding must verify first');
try {
  const altered = JSON.parse(original);
  altered.sha256 = '0'.repeat(64);
  writeFileSync(identityPath, JSON.stringify(altered));
  const result = verify('node-floor');
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /runtime must load the original binding without rebuilding/);
} finally {
  writeFileSync(identityPath, original);
}
const wrongRuntime = verify('node24');
assert.notEqual(wrongRuntime.status, 0);
assert.match(wrongRuntime.stderr, /AssertionError/);
assert.equal(verify('node-floor').status, 0, 'the reference must be restored');
console.log('Runtime identity checks reject a different binary hash and the wrong Node runtime');

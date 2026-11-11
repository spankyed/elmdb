// A process that ends while V8 frees twenty thousand answers it dropped, as `exit.test.mjs`'s child does:
// node probe/stress.cjs BINDING
const { mkdtempSync, rmSync } = require('node:fs');
const { tmpdir } = require('node:os');
const { join } = require('node:path');
const { Database } = require(process.argv[2]);
const dir = mkdtempSync(join(tmpdir(), 'elmdb-stress-'));
const db = Database.open(dir, { syncs: false });
const reads = Buffer.from([0x81, 0xa5, 0x72, 0x65, 0x61, 0x64, 0x73, 0x90]);
for (let i = 0; i < 20000; i++) db.readSync(reads);
db.close();
rmSync(dir, { recursive: true, force: true });
require('node:v8').setFlagsFromString('--expose-gc');
require('node:vm').runInNewContext('gc')({ type: 'minor' });

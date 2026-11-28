// Child process spawned by `panics.test.mjs` with `ELMDB_TEST_PANIC=<export>`.
import assert from 'node:assert/strict';
import { join } from 'node:path';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { Database } = require('../index.js');
const { buildWireBatch } = require('../client.js');

const [, , dir, name] = process.argv;
const schema = { types: { Note: { fields: { title: { type: 'string' } } } }, relations: {} };
const querySpec = { source: { kind: 'type', typeName: 'Note' }, steps: [], terminal: { kind: 'count' } };

const call = async (db) => {
  switch (name) {
    case 'Database.open': return Database.open(dir, { durability: 'unsafe' });
    case 'Database.openAsync': return await Database.openAsync(dir, { durability: 'unsafe' });
    case 'Database.stats': return db.stats();
    case 'Database.get': return db.get('Note-kept');
    case 'Database.query': return db.query(querySpec);
    case 'Database.queryAsync': return await db.queryAsync(querySpec);
    case 'Database.executeWireBatch': return db.executeWireBatch(buildWireBatch((b) => { b.create('Note', { title: 'x' }); }));
    case 'Database.setSchema': return db.setSchema(schema);
    case 'Database.getSchema': return db.getSchema();
    case 'Database.check': return db.check();
    case 'Database.checkAsync': return await db.checkAsync();
    case 'Database.rebuild': return db.rebuild();
    case 'Database.rebuildAsync': return await db.rebuildAsync();
    case 'Database.flush': return db.flush();
    case 'Database.flushAsync': return await db.flushAsync();
    case 'Database.backup': return db.backup(join(dir, 'bak'));
    case 'Database.backupAsync': return await db.backupAsync(join(dir, 'bak-async'));
    case 'Database.close': return db.close();
    case 'transact.swallowed-panic':
      return db.transact(() => {
        db.executeWireBatch(buildWireBatch((b) => { b.create('Note', { title: 'must-not-commit' }, 'Note-leaked'); }));
        try { db.get('Note-kept'); } catch { /* swallow PANICKED */ }
        return 'swallowed';
      });
    case 'beforeCommit.swallowed-panic': {
      db.registerBeforeCommit(['Note'], () => {
        try { db.get('Note-kept'); } catch { /* swallow PANICKED */ }
        return null;
      });
      return db.executeWireBatch(buildWireBatch((b) => { b.create('Note', { title: 'must-not-commit' }, 'Note-leaked'); }));
    }
    default: throw new Error(`unknown test export ${name}`);
  }
};

const isStatic = name === 'Database.open' || name === 'Database.openAsync';
const db = isStatic ? null : Database.open(dir, { durability: 'unsafe' });

let caught;
try {
  await call(db);
} catch (err) {
  caught = err;
}

if (!caught || caught.code !== 'PANICKED') {
  console.error(`${name}: ${caught?.code}`);
  process.exit(1);
}

// Once a handle panics, subsequent calls on that handle are refused with PANICKED until closed, and data survives reopen!
if (db && name !== 'Database.close') {
  assert.throws(() => db.stats(), (err) => err.code === 'PANICKED');
  db.close();
  const reopened = Database.open(dir, { readOnly: true });
  assert.equal(reopened.get('Note-kept').fields.title, 'kept');
  assert.equal(reopened.get('Note-leaked'), null);
  reopened.close();
}

process.stdout.write(`${name} panicked, and the process lives\n`);

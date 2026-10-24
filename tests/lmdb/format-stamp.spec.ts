// New, empty files are stamped with the storage format before anything is written to them. A stamp that fails —
// a full disk — refuses the open: written unstamped, this version's rows would read as format 1 to the next open,
// which would refuse them. Its own file because the failure is made by wrapping `lmdb`'s `open`.
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { afterAll, expect, it, vi } from 'vitest';

let failStamp = true;

vi.mock('lmdb', async (importOriginal) => {
  const lmdb = await importOriginal<typeof import('lmdb')>();
  const open = ((...args: Parameters<typeof lmdb.open>) => {
    const root = lmdb.open(...args);
    const openDB = root.openDB.bind(root);
    root.openDB = ((...dbArgs: Parameters<typeof root.openDB>) => {
      const db = openDB(...dbArgs);
      const options = dbArgs[0] as { name?: string } | undefined;
      if (db && options?.name === 'meta') {
        const putSync = db.putSync.bind(db);
        db.putSync = ((key: unknown, ...rest: unknown[]) => {
          if (failStamp && key === 'format') throw new Error('No space left on device');
          return (putSync as (...a: unknown[]) => unknown)(key, ...rest);
        }) as typeof db.putSync;
      }
      return db;
    }) as typeof root.openDB;
    return root;
  }) as typeof lmdb.open;
  return { ...lmdb, open };
});

const { openEnvironment, LMDB_FORMAT_VERSION } = await import('../../src/lmdb/store.ts');

const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ears-format-stamp-'));
afterAll(() => fs.rmSync(dir, { recursive: true, force: true }));

it('refuses new, empty files it could not stamp, and stamps them once it can', () => {
  const basePath = path.join(dir, 'ears-db');
  expect(() => openEnvironment(basePath)).toThrow(/Couldn't record the storage format of .*No space left on device/);

  failStamp = false;
  const env = openEnvironment(basePath);
  try {
    expect(env.root.openDB({ name: 'meta', encoding: 'json' }).get('format')).toBe(LMDB_FORMAT_VERSION);
  } finally {
    env.root.close();
  }
});

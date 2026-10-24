// The engine the contract specs run against. The specs only reach it through this file, so the same specs cover the
// engine on each backend: in memory by default, and with `EARS_KV=lmdb` (the `ears-on-lmdb` project) on an LMDB store
// in a directory of its own, removed when the test file ends.
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { afterAll } from 'vitest';
import { createEarsEngine, type EARSRuntimeDeps, type EarsEngine } from '../../src/index.ts';
import { openLmdbStore, type LmdbStore } from '../../src/lmdb/index.ts';

export interface EngineUnderTestOptions {
  isEntityType: (name: string) => boolean;
  isRelationKind: (name: string) => boolean;
  isRole: (name: string) => boolean;
  rowRules?: EARSRuntimeDeps['rowRules'];
  warn?: EARSRuntimeDeps['warn'];
  kv?: EARSRuntimeDeps['kv'];
}

const onLmdb = process.env.EARS_KV === 'lmdb';
const stores: Array<{ store: LmdbStore; dir: string }> = [];
if (onLmdb) {
  afterAll(() => {
    for (const { store, dir } of stores.splice(0)) {
      store.close();
      fs.rmSync(dir, { recursive: true, force: true });
    }
  });
}

/** A store for one engine: on LMDB in a directory of its own when the specs run there, else none (memory) */
function lmdbStore() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ears-engine-'));
  const store = openLmdbStore({ path: path.join(dir, 'ears-db') });
  stores.push({ store, dir });
  return store.kv;
}

export function freshEngine(options: EngineUnderTestOptions): EarsEngine {
  return createEarsEngine({ ...options, kv: options.kv ?? (onLmdb ? lmdbStore() : undefined) });
}

export type EngineUnderTest = EarsEngine;

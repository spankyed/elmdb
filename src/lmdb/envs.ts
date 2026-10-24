/// <reference types="node" />
import { open, type Database, type RootDatabase } from 'lmdb';
import * as fs from 'node:fs';
import * as path from 'node:path';
import { checkStamp } from './stamp.ts';

/** The databases of one LMDB environment: entity records, attribute rows and relations */
export type LmdbDbs = {
  entities: Database<any>;
  attrs: Database<any>;
  relations: Database<any>;
  root: RootDatabase;
};

/**
 * How this version of `@apackdev/ears` encodes rows in LMDB (keys, attribute and relation records). An environment
 * records the format it was written in; one in another format is refused rather than misread.
 *
 * Format 3 keeps one record of each fact, and one row per attribute: an entity's row holds only its type, written
 * when it is created; an attribute is one row keyed `kind␟entityId` holding its one value (a list is one array
 * value); a relation is its row in the relations table, with no attribute copy and no entity row; `entityType` is
 * read from the id rather than stored. There is no upgrade in the app: `scripts/ears-convert.ts` converts a copy of
 * an older data dir, once, explicitly (`convertToCurrentFormat`).
 */
export const LMDB_FORMAT_VERSION = 3;

/** Whether any of the environment's data databases holds a row */
function holdsData(root: RootDatabase): boolean {
  for (const name of ['entities', 'attrs', 'relations']) {
    const db = root.openDB({ name, encoding: 'json' }) as Database<unknown> | undefined;
    if (db) for (const _key of db.getKeys({ limit: 1 })) return true;
  }
  return false;
}

/**
 * Opens (creating it if needed) the LMDB environment at `basePath`; `readOnly` opens an existing one without writing.
 * Throws when the environment is in another storage format (`LMDB_FORMAT_VERSION`).
 */
export function openEnvAt(basePath: string, { readOnly = false }: { readOnly?: boolean } = {}): LmdbDbs {
  // LMDB would create the files before failing, so a directory with no database in it is refused here
  if (readOnly && !fs.existsSync(path.join(basePath, 'data.mdb'))) throw new Error(`No LMDB database at ${basePath}`);
  // Ensure parent directory exists
  const parentDir = path.dirname(basePath);
  if (!readOnly && !fs.existsSync(parentDir)) {
    fs.mkdirSync(parentDir, { recursive: true });
  }

  const root = open({
    path: basePath,
    maxDbs: 8,
    compression: true,
    readOnly,
  });
  try {
    checkStamp(root, basePath, readOnly, {
      format: LMDB_FORMAT_VERSION,
      noun: 'database',
      // Data that records no format was written before formats were recorded, which is format 1
      unstamped: (env) => (holdsData(env) ? 1 : undefined),
      older: 'convert a copy of it with scripts/ears-convert.ts',
    });
  } catch (error) {
    root.close();
    throw error;
  }

  return {
    entities: root.openDB({ name: 'entities', encoding: 'json' }),
    attrs: root.openDB({ name: 'attrs', encoding: 'json' }),
    relations: root.openDB({ name: 'relations', encoding: 'json' }),
    root,
  };
}

/** Whether `error` says only that a database was already closed, which closing it again is allowed to meet */
export const isAlreadyClosed = (error: unknown) =>
  error instanceof Error && (error.message.includes('Dbi is not open') || error.message.includes('already been closed'));

/**
 * Closes an environment's databases, then its root, so the root closes even when a child was closed already.
 * Throws anything else it meets: what to do with it is the caller's.
 */
export function closeEnv(dbs: LmdbDbs): void {
  try {
    dbs.entities?.close();
    dbs.attrs?.close();
    dbs.relations?.close();
    dbs.root?.close();
  } catch (error) {
    if (!isAlreadyClosed(error)) throw error;
  }
}

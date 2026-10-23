/// <reference types="node" />
import { open, type Database, type RootDatabase } from 'lmdb';
import * as fs from 'node:fs';
import * as path from 'node:path';
import type { Partition } from '../persistence/policy.ts';

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
 * Format 2 keeps one record of each fact: an entity's row holds only its type, written when it is created; a
 * relation is its row in the relations table, with no attribute copy and no entity row; `entityType` is read from
 * the id rather than stored. Format 1 also stored those copies, and they had drifted apart. There is no upgrade in
 * the app: `scripts/ears-format-2.ts` converts a copy of a format 1 data dir, once, explicitly.
 */
export const LMDB_FORMAT_VERSION = 2;

/** The environment's own records, beside its data: its storage format */
const META_DB = 'meta';
const FORMAT_KEY = 'format';

/**
 * Checks the environment's storage format, and records it in a new, empty one unless it's read-only. Data that
 * records no format was written before formats were recorded, which is format 1. Throws for any format but this
 * one.
 */
function checkFormat(root: RootDatabase, basePath: string, readOnly: boolean): void {
  // A read-only environment has no database the writer never created, and one written before formats were recorded
  // has none either
  const meta = root.openDB({ name: META_DB, encoding: 'json' }) as Database<unknown> | undefined;
  let format = meta?.get(FORMAT_KEY);
  if (format === undefined) {
    if (holdsData(root)) {
      format = 1;
    } else {
      // An empty environment is stamped before anything is written to it, and a stamp that fails refuses the open:
      // the next open would read this version's writes, unstamped, as format 1 and refuse them. Refusing now costs
      // nothing, since there is nothing in it yet
      if (!readOnly && meta) {
        try {
          meta.putSync(FORMAT_KEY, LMDB_FORMAT_VERSION);
        } catch (error) {
          throw new Error(`Couldn't record the storage format of ${basePath}, so it isn't opened: ${(error as Error).message}`);
        }
      }
      return;
    }
  }
  if (format !== LMDB_FORMAT_VERSION) {
    const newer = typeof format === 'number' && format > LMDB_FORMAT_VERSION;
    throw new Error(
      `The database at ${basePath} is in storage format ${JSON.stringify(format)}, but this version reads format ${LMDB_FORMAT_VERSION}` +
      (newer ? ': it was written by a newer apack, so update this one'
        : format === 1 ? ': convert a copy of it with scripts/ears-format-2.ts' : ''),
    );
  }
}

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
    checkFormat(root, basePath, readOnly);
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

/** Each partition's database directory */
export type LmdbPaths = Record<Partition, string>;

/** Opens each partition's environment; `readOnly` opens existing ones without writing */
export function openShardedEnvs(paths: LmdbPaths, { readOnly = false }: { readOnly?: boolean } = {}): Record<Partition, LmdbDbs> {
  const primary = openEnvAt(paths.primary, { readOnly });
  try {
    return { primary, volatileBackup: openEnvAt(paths.volatileBackup, { readOnly }) };
  } catch (error) {
    // Neither partition stays open when one of them can't be
    closeEnv(primary, () => {});
    // The volatile partition holds run history and nothing else, which is worth saying before someone despairs of a
    // database that is otherwise fine
    throw new Error(`${(error as Error).message}\n  It holds the run history only: deleting ${paths.volatileBackup} loses that and nothing else.`, { cause: error });
  }
}

export function closeShardedEnvs(envs: Record<Partition, LmdbDbs>, log: (message: string) => void = console.log) {
  closeEnv(envs.primary, log);
  closeEnv(envs.volatileBackup, log);
}

export function closeEnv(dbs: LmdbDbs, log: (message: string) => void = console.log): void {
  try {
    // Close child databases first, then root
    // This ensures clean shutdown even if root close fails
    dbs.entities?.close();
    dbs.attrs?.close();
    dbs.relations?.close();
    dbs.root?.close();
    log('[LMDB] Environment closed successfully');
  } catch (error: any) {
    // Only log unexpected errors (not "already closed" errors)
    if (!error?.message?.includes('Dbi is not open') &&
        !error?.message?.includes('already been closed')) {
      console.error('[LMDB] Error closing environment:', error);
    }
  }
}

/** Delete all LMDB database directories. Must be called after connections are closed. */
export function deleteLmdbDirectories(paths: LmdbPaths): void {
  for (const dbPath of [paths.primary, paths.volatileBackup]) {
    if (fs.existsSync(dbPath)) {
      fs.rmSync(dbPath, { recursive: true, force: true });
    }
  }
}

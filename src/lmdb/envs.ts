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
 * How this version of `@apack/ears` encodes rows in LMDB (keys, attribute and relation records). An environment
 * records the format it was written in; one in another format is refused rather than misread. Change the encoding,
 * and this number goes up with code that upgrades the older format.
 */
export const LMDB_FORMAT_VERSION = 1;

/** The environment's own records, beside its data: its storage format */
const META_DB = 'meta';
const FORMAT_KEY = 'format';

/**
 * Checks the environment's storage format, and records it in one that has none (written before formats were
 * recorded, which is format 1) unless it's read-only. Throws for any other format.
 */
function checkFormat(root: RootDatabase, basePath: string, readOnly: boolean): void {
  // A read-only environment has no database the writer never created, and one written before formats were recorded
  // has none either
  const meta = root.openDB({ name: META_DB, encoding: 'json' }) as Database<unknown> | undefined;
  const format = meta?.get(FORMAT_KEY);
  if (format === undefined) {
    // Recording it is what a writer can do for the next reader, not a condition of opening: a full disk mustn't
    // stop an app opening a database it could read before
    if (!readOnly && meta) {
      try {
        meta.putSync(FORMAT_KEY, LMDB_FORMAT_VERSION);
      } catch (error) {
        console.warn(`[LMDB] Couldn't record the storage format of ${basePath}:`, (error as Error).message);
      }
    }
    return;
  }
  if (format !== LMDB_FORMAT_VERSION) {
    const newer = typeof format === 'number' && format > LMDB_FORMAT_VERSION;
    throw new Error(
      `The database at ${basePath} is in storage format ${JSON.stringify(format)}, but this version reads format ${LMDB_FORMAT_VERSION}` +
      (newer ? ': it was written by a newer apack, so update this one' : ''),
    );
  }
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

/// <reference types="node" />
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { Database, type DumpReport, type OpenOptions, type Schema } from 'elmdb';
import type { BuildPolicy, DurabilityMode, EarsDatabase } from '../database.ts';
import { createEarsEngine, type EarsEngine } from '../engine.ts';
import { toEarsError, type EarsError } from '../errors.ts';
import type { EarsClock } from '../watch.ts';

/** Canonical elmdb v2 on-disk format identifier. */
export const DATABASE_FORMAT = 'elmdb/2';

/** Options for opening an elmdb v2 database via `@elmdb/ears/database`. */
export interface DatabaseOptions {
  /** Database directory path (created unless `readOnly` is `true`). */
  readonly path: string;
  /** Schema to apply/evolve on open (omitted keeps stored schema; disallowed with `readOnly`). */
  readonly schema?: Schema;
  /** Opens existing database files in strict `MDB_RDONLY` read-only mode. */
  readonly readOnly?: boolean;
  /** Durability mode: `'strict'` (default), `'grouped'`, or `'unsafe'` (ephemeral/tests). */
  readonly durability?: DurabilityMode;
  /** Index build policy: `'deferred'` (default) or `'eager'`. */
  readonly buildPolicy?: BuildPolicy;
  readonly mapSizeBytes?: number;
  readonly maxDbs?: number;
  readonly maxReaders?: number;
  readonly planCacheCapacity?: number;
  readonly watchTurnSliceMs?: number;
}

/** Options for opening an `EarsEngine` directly in one call. */
export interface OpenEngineOptions extends DatabaseOptions {
  readonly clock?: EarsClock;
  readonly onWatchError?: (error: EarsError) => void;
}

function toNativeOpenOptions(options: DatabaseOptions): OpenOptions {
  const out: OpenOptions = {};
  if (options.readOnly !== undefined) out.readOnly = options.readOnly;
  if (options.schema !== undefined) out.schema = options.schema;
  if (options.durability !== undefined) out.durability = options.durability;
  if (options.buildPolicy !== undefined) out.buildPolicy = options.buildPolicy;
  if (options.mapSizeBytes !== undefined) out.mapSizeBytes = options.mapSizeBytes;
  if (options.maxDbs !== undefined) out.maxDbs = options.maxDbs;
  if (options.maxReaders !== undefined) out.maxReaders = options.maxReaders;
  if (options.planCacheCapacity !== undefined) out.planCacheCapacity = options.planCacheCapacity;
  if (options.watchTurnSliceMs !== undefined) out.watchTurnSliceMs = options.watchTurnSliceMs;
  return out;
}

/**
 * Opens an elmdb v2 database at `options.path` and returns it as an `EarsDatabase` with zero casts.
 */
export function openDatabase(options: DatabaseOptions): EarsDatabase {
  try {
    return Database.open(options.path, toNativeOpenOptions(options));
  } catch (error) {
    throw toEarsError(error);
  }
}

/**
 * Opens an elmdb v2 database asynchronously on Node's worker pool.
 */
export async function openDatabaseAsync(options: DatabaseOptions): Promise<EarsDatabase> {
  try {
    return await Database.openAsync(options.path, toNativeOpenOptions(options));
  } catch (error) {
    throw toEarsError(error);
  }
}

/** Opens an elmdb v2 database and wraps it in an `EarsEngine` in a single call. */
export function openEngine(options: OpenEngineOptions): EarsEngine {
  const database = openDatabase(options);
  return createEarsEngine({
    database,
    ...(options.clock !== undefined && { clock: options.clock }),
    ...(options.onWatchError !== undefined && { onWatchError: options.onWatchError }),
  });
}

/** Opens an elmdb v2 database asynchronously and wraps it in an `EarsEngine`. */
export async function openEngineAsync(options: OpenEngineOptions): Promise<EarsEngine> {
  const database = await openDatabaseAsync(options);
  return createEarsEngine({
    database,
    ...(options.clock !== undefined && { clock: options.clock }),
    ...(options.onWatchError !== undefined && { onWatchError: options.onWatchError }),
  });
}

/** Loads a portable elmdb v2 JSON dump file into a brand-new database directory. */
export function loadDump(dumpFile: string, targetDir: string): DumpReport {
  try {
    return Database.load(dumpFile, targetDir);
  } catch (error) {
    throw toEarsError(error);
  }
}

/** Temporary database handle created by `createTemporaryDatabases()`. */
export interface TemporaryDatabaseHandle {
  readonly path: string;
  readonly database: EarsDatabase;
  dispose(): void;
}

/** Factory for isolated temporary elmdb v2 databases (tests & ephemeral tooling). */
export interface TemporaryDatabases {
  open(options?: Omit<DatabaseOptions, 'path'>): TemporaryDatabaseHandle;
  disposeAll(): void;
}

/**
 * Creates a temporary database registry whose directories are removed on `dispose()`,
 * `disposeAll()`, or best-effort on process exit.
 */
export function createTemporaryDatabases(): TemporaryDatabases {
  const open = new Map<string, EarsDatabase>();
  let listening = false;

  const detach = (): void => {
    if (!listening) return;
    listening = false;
    process.off('exit', atExit);
  };

  const remove = (dir: string): void => {
    const database = open.get(dir);
    if (!database) return;
    open.delete(dir);
    if (open.size === 0) detach();
    let failure: unknown;
    try {
      database.close();
    } catch (error) {
      failure = error;
    }
    try {
      fs.rmSync(dir, { recursive: true, force: true });
    } catch (error) {
      if (failure === undefined) failure = error;
    }
    if (failure !== undefined) throw failure;
  };

  const atExit = (): void => {
    listening = false;
    for (const dir of [...open.keys()]) {
      try {
        remove(dir);
      } catch {
        // Best-effort cleanup at process exit.
      }
    }
  };

  const disposeAll = (): void => {
    const errors: unknown[] = [];
    for (const dir of [...open.keys()]) {
      try {
        remove(dir);
      } catch (error) {
        errors.push(error);
      }
    }
    detach();
    if (errors.length === 1) throw errors[0];
    if (errors.length > 1) throw new AggregateError(errors, 'Failed to dispose multiple temporary databases');
  };

  return {
    open(options?: Omit<DatabaseOptions, 'path'>): TemporaryDatabaseHandle {
      const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ears-v2-'));
      let database: EarsDatabase;
      try {
        database = openDatabase({
          durability: 'unsafe',
          buildPolicy: 'eager',
          ...options,
          path: dir,
        });
      } catch (error) {
        fs.rmSync(dir, { recursive: true, force: true });
        throw error;
      }
      open.set(dir, database);
      if (!listening) {
        listening = true;
        process.once('exit', atExit);
      }
      return { path: dir, database, dispose: () => remove(dir) };
    },
    disposeAll,
  };
}

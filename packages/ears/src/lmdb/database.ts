/// <reference types="node" />
// Opening the elmdb database an engine runs on: the one module that loads elmdb, which only whoever creates an engine
// (a program's composition root, its tools and tests) reaches, through this entry.
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import elmdb from 'elmdb';
import type { EarsDatabase } from '../database.ts';
import { toElmdbSchema, type EngineSchema } from '../schema.ts';

/** The storage format an elmdb database is in, as a backup records it: provenance, since opening is what decides */
export const DATABASE_FORMAT = 'elmdb/1';

export interface DatabaseOptions {
  /** The database's directory, created unless `readOnly` */
  path: string;
  /** The schema to hold the database to: a schema that differs from the stored one changes it. None keeps the stored one */
  schema?: EngineSchema;
  /** Opens existing files without writing: every write is refused, and the stored schema is used */
  readOnly?: boolean;
  /**
   * Group commit's syncs, on by default: a commit is on disk within a second, and `durable()` waits for one. Off,
   * periodic syncing is disabled for temporary data. Creating an environment still durably initializes its format
   */
  syncs?: boolean;
  /**
   * Sync every commit as it is made, so that one that returned survives the machine losing power, at the price of a
   * sync per commit. It syncs whatever `syncs` says, and is ignored by a read-only database. Off by default
   */
  syncEachCommit?: boolean;
  /**
   * Group commit syncs once writes have paused for this many milliseconds: 100 by default. A whole number, or refused
   * (`VALUE_INVALID`)
   */
  syncIdleMs?: number;
  /** Group commit syncs no later than this many milliseconds after the first commit not yet synced: 1000 by default */
  syncIntervalMs?: number;
  /**
   * The map's size at open: by default twice the data file, and 64 MiB at least. A whole number of 1 at least, or
   * refused (`VALUE_INVALID`), and a multiple of the system's page size, or refused by LMDB's binding (`IO`)
   */
  mapSizeBytes?: number;
}

/**
 * Opens the elmdb database at `path` for an engine (`createEarsEngine({ database })`): the binding's `Database`, which
 * is an `EarsDatabase` as it is, with no cast (`port.test-d.ts`)
 */
export function openDatabase({
  path: dir, schema, readOnly, syncs, syncEachCommit, syncIdleMs, syncIntervalMs, mapSizeBytes,
}: DatabaseOptions): EarsDatabase {
  return elmdb.Database.open(dir, {
    ...(readOnly !== undefined && { readOnly }),
    // elmdb checks the schema whole, naming every problem
    ...(schema && { schema: toElmdbSchema(schema) }),
    ...(syncs !== undefined && { syncs }),
    ...(syncEachCommit !== undefined && { syncEachCommit }),
    ...(syncIdleMs !== undefined && { syncIdleMs }),
    ...(syncIntervalMs !== undefined && { syncIntervalMs }),
    ...(mapSizeBytes !== undefined && { mapSizeBytes }),
  });
}

/** Databases opened in temporary directories, each removed when it is disposed or the process exits */
export interface TemporaryDatabases {
  /** A new database in its own temporary directory, with periodic syncing disabled; `dispose` closes it and removes the directory */
  open(schema: EngineSchema): { database: EarsDatabase; dispose(): void };
  /** Closes and removes every database still open */
  disposeAll(): void;
}

/**
 * Temporary databases: what a test runtime, or tooling working on a copy, opens. Their directories are removed as each
 * is disposed, and those still there when the process exits are removed then
 */
export function createTemporaryDatabases(): TemporaryDatabases {
  const open = new Map<string, EarsDatabase>();
  let listening = false;
  const detach = () => {
    if (!listening) return;
    listening = false;
    process.off('exit', atExit);
  };
  const remove = (dir: string) => {
    if (!open.has(dir)) return;
    const database = open.get(dir);
    open.delete(dir);
    if (open.size === 0) detach();
    let failure: unknown;
    if (database?.isOpen) {
      try { database.close(); } catch (error) { failure = error; }
    }
    try {
      fs.rmSync(dir, { recursive: true, force: true });
    } catch (error) {
      if (!failure) failure = error;
    }
    if (failure) throw failure;
  };
  const disposeAll = () => {
    const errors: unknown[] = [];
    for (const dir of [...open.keys()]) {
      try { remove(dir); } catch (error) { errors.push(error); }
    }
    detach();
    if (errors.length === 1) throw errors[0];
    if (errors.length > 1) throw new AggregateError(errors, 'Failed to dispose multiple temporary databases');
  };
  const atExit = () => {
    listening = false;
    for (const dir of [...open.keys()]) {
      try { remove(dir); } catch { /* Best-effort cleanup at process exit */ }
    }
  };
  return {
    open(schema) {
      const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ears-'));
      let database: EarsDatabase;
      try {
        database = openDatabase({ path: dir, schema, syncs: false });
      } catch (error) {
        fs.rmSync(dir, { recursive: true, force: true });
        throw error;
      }
      open.set(dir, database);
      if (!listening) {
        listening = true;
        process.once('exit', atExit);
      }
      return { database, dispose: () => remove(dir) };
    },
    disposeAll,
  };
}

/// <reference types="node" />
// Opening the elmdb database an engine runs on: the one module that loads elmdb, which only the app, its tools and
// tests reach through this entry.
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import elmdb, { type Schema } from 'elmdb';
import type { EarsDatabase } from '../database.ts';
import { toElmdbSchema, type EngineSchema } from '../schema.ts';

/** The storage format an elmdb database is in, as a backup records it: provenance, since opening is what decides */
export const DATABASE_FORMAT = 'elmdb/2';

export interface DatabaseOptions {
  /** The database's directory, created unless `readOnly` */
  path: string;
  /** The schema to hold the database to: a schema that differs from the stored one changes it. None keeps the stored one */
  schema?: EngineSchema;
  /** Opens existing files without writing: every write is refused, and the stored schema is used */
  readOnly?: boolean;
  /**
   * Group commit's syncs, on by default: a commit is on disk within a second, and `durable()` waits for one. Off,
   * nothing is synced, for a database in a temporary directory that a machine's crash can't matter to
   */
  syncs?: boolean;
}

/** Opens the elmdb database at `path` for an engine (`createEarsEngine({ database })`) */
export function openDatabase({ path: dir, schema, readOnly, syncs }: DatabaseOptions): EarsDatabase {
  return elmdb.Database.open(dir, {
    ...(readOnly && { readOnly }),
    // elmdb checks the schema whole, naming every problem
    ...(schema && { schema: toElmdbSchema(schema) as Schema }),
    ...(syncs === false && { syncs }),
  }) as unknown as EarsDatabase;
}

/** Databases opened in temporary directories, each removed when it is disposed or the process exits */
export interface TemporaryDatabases {
  /** A new database in a temporary directory of its own, with no syncs; `dispose` closes it and removes the directory */
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
  const remove = (dir: string) => {
    const database = open.get(dir);
    open.delete(dir);
    if (database?.isOpen) database.close();
    fs.rmSync(dir, { recursive: true, force: true });
  };
  const atExit = () => {
    for (const dir of open.keys()) remove(dir);
  };
  process.once('exit', atExit);
  return {
    open(schema) {
      const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ears-'));
      const database = openDatabase({ path: dir, schema, syncs: false });
      open.set(dir, database);
      return { database, dispose: () => remove(dir) };
    },
    disposeAll: atExit,
  };
}

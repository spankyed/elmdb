import * as fs from 'node:fs';
import * as path from 'node:path';
import { open, type Database, type RootDatabase } from 'lmdb';
import { createOrderedKV, type Key, type OrderedKV } from '../kv.ts';
import { createLmdbKVBackend } from './kv-backend.ts';
import { checkStamp } from './stamp.ts';

/**
 * How this version of `@apackdev/ears` lays out an environment: one database, `kv`, holding the engine's keys
 * (`attribute-storage.ts`, `relations.ts`) in the seam's codec, with `msgpackr` values. An environment records the
 * format it was written in, and one in another format is refused rather than misread: an older one is converted
 * explicitly, on a copy, with `scripts/ears-convert.ts` (`convertToCurrentFormat`).
 */
export const LMDB_FORMAT_VERSION = 4;

/** How long a commit waits, at most, for the sync that makes it durable: group commit's window */
export const SYNC_INTERVAL_MS = 200;

/** The databases an older format kept: holding a row, an environment that records no format is format 1 */
const OLDER_DATABASES = ['entities', 'attrs', 'relations'];

/** Whether any database of an older format holds a row */
function holdsOlderData(root: RootDatabase): boolean {
  for (const name of OLDER_DATABASES) {
    const db = root.openDB({ name, encoding: 'json' }) as Database<unknown> | undefined;
    if (db) for (const _key of db.getKeys({ limit: 1 })) return true;
  }
  return false;
}

/** An environment as this version opens it: its root and its one database */
export interface LmdbEnvironment {
  root: RootDatabase;
  db: Database<Uint8Array, Uint8Array>;
}

/**
 * Opens the environment at `dir` — the one way it is opened, so every process sharing it uses the same flags — after
 * checking it records this version's format, and stamping a new one. `readOnly` opens existing files without writing.
 */
export function openEnvironment(dir: string, { readOnly = false }: { readOnly?: boolean } = {}): LmdbEnvironment {
  // LMDB would create the files before failing, so a directory with no database in it is refused here
  if (readOnly && !fs.existsSync(path.join(dir, 'data.mdb'))) throw new Error(`No LMDB database at ${dir}`);
  if (!readOnly) fs.mkdirSync(path.dirname(dir), { recursive: true });
  // `overlappingSync` off: a unit committing without a sync returns without waiting for one (on, even those wait),
  // and every process opening an environment has to agree on it
  const root = open({ path: dir, maxDbs: 8, overlappingSync: false, readOnly });
  try {
    checkStamp(root, dir, readOnly, {
      format: LMDB_FORMAT_VERSION,
      noun: 'database',
      unstamped: (env) => (holdsOlderData(env) ? 1 : undefined),
      older: 'convert a copy of it with scripts/ears-convert.ts',
    });
    return { root, db: root.openDB({ name: 'kv', keyEncoding: 'binary', encoding: 'binary', compression: false }) as Database<Uint8Array, Uint8Array> };
  } catch (error) {
    root.close();
    throw error;
  }
}

/** An app's LMDB store: the environment the engine reads and writes through `kv`, and the store as a whole */
export interface LmdbStore {
  /** The store an engine is created on (`createEarsEngine({ kv: store.kv })`) */
  readonly kv: OrderedKV;
  /** The database directory */
  readonly path: string;
  readonly readOnly: boolean;
  /** How many syncs have run: each made every commit before it durable */
  readonly syncs: number;
  /**
   * How many of the environment's reader slots are taken: open read snapshots, which this process's reads share. A
   * cursor or snapshot left open holds one for the life of the process
   */
  readers(): number;
  /**
   * Resolves once every unit committed before the call is on disk. A unit is visible, and survives the process
   * crashing, as soon as it commits; a sync is what survives the machine failing. Group commit syncs within
   * `SYNC_INTERVAL_MS` of a commit on its own; this is for a caller that must not go on before then
   */
  durable(): Promise<void>;
  /** Asks for a sync now when anything is waiting for one, without waiting for it: a user's action committed */
  requestSync(): void;
  /**
   * Writes a consistent copy of the database into `targetDir` (as `data.mdb`): one read transaction's worth, so a
   * copy made while the app writes is one moment of the database
   */
  copyTo(targetDir: string): Promise<void>;
  /** Replaces every entry with those of the database at `dir`, in one unit: a reader sees the old or the new */
  replaceWith(dir: string): void;
  /** Removes every entry, in one unit */
  reset(): void;
  /** Syncs and closes the environment; a store closed refuses reads and writes */
  close(): void;
}

export interface LmdbStoreOptions {
  /** The database directory */
  path: string;
  /** Opens existing files without writing to them: a write throws */
  readOnly?: boolean;
  /** How long a commit waits, at most, for its sync; `SYNC_INTERVAL_MS` by default */
  syncIntervalMs?: number;
}

/**
 * Opens the LMDB store at `path`. A unit commits without a sync and is visible at once; syncs follow within
 * `syncIntervalMs`, run off the main thread, each making every commit before it durable (group commit).
 */
export function openLmdbStore({ path: dir, readOnly = false, syncIntervalMs = SYNC_INTERVAL_MS }: LmdbStoreOptions): LmdbStore {
  const { root, db } = openEnvironment(dir, { readOnly });
  let closed = false;
  let syncs = 0;
  /** Commits made since the last sync began */
  let owed = false;
  /** The sync running, and the one asked for while it runs */
  let running: Promise<void> | undefined;
  let queued: Promise<void> | undefined;
  let timer: ReturnType<typeof setTimeout> | undefined;

  /** Runs a sync off the main thread; the commits made before it began are on disk when it resolves */
  const runSync = (): Promise<void> => {
    owed = false;
    if (timer) { clearTimeout(timer); timer = undefined; }
    const done = new Promise<void>((resolve, reject) => {
      // `sync` is lmdb-js's (its environment's mdb_env_sync, run off the main thread), missing from its types
      (root as unknown as { sync(done: (error?: Error) => void): void }).sync((error?: Error) => {
        syncs++;
        if (error) reject(error);
        else resolve();
      });
    });
    // What the next caller waits on: settled either way, the error being this caller's to see
    running = done.then(() => {}, () => {}).finally(() => { running = undefined; });
    return done;
  };

  /** A sync that covers every commit made so far: the one running when nothing was committed since, else a new one */
  const sync = (): Promise<void> => {
    if (closed) return Promise.resolve();
    if (!owed) return running ?? Promise.resolve();
    if (!running) return runSync();
    // A sync is running and began before the latest commits: one more follows it
    queued ??= running.then(() => { queued = undefined; return owed ? runSync() : undefined; });
    return queued;
  };

  const backend = createLmdbKVBackend(root, db, {
    committed() {
      owed = true;
      if (!timer && !closed) {
        timer = setTimeout(() => { timer = undefined; void sync(); }, syncIntervalMs);
        // The timer keeps no process alive: closing syncs what is owed
        timer.unref?.();
      }
    },
    synced() { owed = false; },
    durable: sync,
  });
  const refuseReadOnly = (what: string) => {
    if (readOnly) throw new Error(`The LMDB store at ${dir} is open read-only: it can't ${what}`);
  };
  // LMDB opened read-only has no write transaction to take, so a write is refused here, naming why
  const kv = createOrderedKV(readOnly ? { ...backend, write: () => refuseReadOnly('be written') as never } : backend);

  /** Writes the marker of a durable commit: a unit with no write wouldn't commit, nor so sync */
  const SYNCED = ['m', 'synced'] as const;

  /** Every key in the database, collected, since the body removes them */
  const everyKey = (database: Database<Uint8Array, Uint8Array>) => [...database.getKeys()] as Uint8Array[];

  return {
    kv,
    path: dir,
    readOnly,
    get syncs() { return syncs; },
    // One line per slot, under a header
    readers: () => root.readerList().trim().split('\n').slice(1).length,
    durable: sync,
    requestSync() { void sync(); },
    async copyTo(targetDir) {
      // LMDB writes data.mdb into a directory that already exists, and no lock.mdb: it makes one when opened
      fs.mkdirSync(targetDir, { recursive: true });
      await root.backup(targetDir, false);
    },
    replaceWith(source) {
      refuseReadOnly('be replaced');
      const from = openEnvironment(source, { readOnly: true });
      try {
        backend.write((w) => {
          for (const key of everyKey(db)) w.del(key);
          for (const { key, value } of from.db.getRange()) w.put(key as Uint8Array, value as Uint8Array);
        }, { durable: true });
      } finally {
        from.root.close();
      }
    },
    reset() {
      refuseReadOnly('be reset');
      kv.write((w) => {
        const keys: Key[] = [];
        w.range({ values: false }, (key) => { keys.push(key); });
        for (const key of keys) w.del(key);
        w.put(SYNCED, Date.now());
      }, { durable: true });
    },
    close() {
      if (closed) return;
      closed = true;
      if (timer) clearTimeout(timer);
      // What is owed is synced before the files close: closing doesn't sync on its own
      if (owed && !readOnly) kv.write((w) => w.put(SYNCED, Date.now()), { durable: true });
      root.close();
    },
  };
}

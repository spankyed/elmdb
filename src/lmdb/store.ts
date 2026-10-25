import * as fs from 'node:fs';
import * as path from 'node:path';
import { open, type Database, type RootDatabase } from 'lmdb';
import { createOrderedKV, type OrderedKV } from '../kv.ts';
import { createLmdbKVBackend, syncEnvironment } from './kv-backend.ts';
import { checkStamp } from './stamp.ts';

/**
 * How this version of `@apackdev/ears` lays out an environment: one database, `kv`, holding the engine's keys
 * (`attribute-storage.ts`, `relations.ts`) in the seam's codec, with `msgpackr` values. An environment records the
 * format it was written in, and one in another format is refused rather than misread: an older one is converted
 * explicitly, on a copy, with `scripts/ears-convert.ts` (`convertToCurrentFormat`).
 */
export const LMDB_FORMAT_VERSION = 4;

/** How long writes must pause before a sync runs: a burst of commits is made durable by one sync after it */
export const SYNC_IDLE_MS = 100;

/**
 * Whether a copy (`copyTo`, a conversion's `compactEnvironment`) is compacted. Off: lmdb-js 3.5.9 aborts the process
 * when a compacted copy of multi-page values is emptied (kriszyp/lmdb-js#381,
 * `docs/references/evidence/lmdb-compacted-copy-crash.md`). True once `lmdb` is a release with that fix.
 */
export const COMPACT_COPIES = false;

/** How long a commit waits, at most, for the sync that makes it durable, however steadily writes go on */
export const SYNC_INTERVAL_MS = 1000;

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
  /** How many syncs have started: each makes every commit before it durable */
  readonly syncs: number;
  /**
   * How many of the environment's reader slots are taken: open read snapshots, which this process's reads share. A
   * cursor or snapshot left open holds one for the life of the process
   */
  readers(): number;
  /**
   * Resolves once every unit committed before the call is on disk. A unit is visible, and survives the process
   * crashing, as soon as it commits; a sync is what survives the machine failing. Group commit syncs once writes
   * pause for `SYNC_IDLE_MS`, and within `SYNC_INTERVAL_MS` of the first unsynced commit at the latest; this is for a
   * caller that must not go on before then
   */
  durable(): Promise<void>;
  /**
   * Writes a consistent copy of the database into `targetDir` (as `data.mdb`): one read transaction's worth, so a
   * copy made while the app writes is one moment of the database. The copy is compacted: it holds the pages in use
   * and none of those deletes freed, which the store's own file keeps for later writes
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
  /** How long writes must pause before a sync runs; `SYNC_IDLE_MS` by default */
  syncIdleMs?: number;
  /** How long a commit waits, at most, for its sync; `SYNC_INTERVAL_MS` by default */
  syncIntervalMs?: number;
}

/**
 * Opens the LMDB store at `path`. A unit commits without a sync and is visible at once; a sync follows once writes
 * pause for `syncIdleMs`, or `syncIntervalMs` after the first unsynced commit while they don't, run off the main
 * thread, each making every commit before it durable (group commit).
 */
export function openLmdbStore({ path: dir, readOnly = false, syncIdleMs = SYNC_IDLE_MS, syncIntervalMs = SYNC_INTERVAL_MS }: LmdbStoreOptions): LmdbStore {
  const { root, db } = openEnvironment(dir, { readOnly });
  let closed = false;
  let syncs = 0;
  /** Commits made since the last sync began */
  let owed = false;
  /** The sync running, which rejects as it fails, and the one asked for while it runs */
  let running: Promise<void> | undefined;
  let queued: Promise<void> | undefined;
  /** The sync due once writes pause, which each commit pushes back, and the one due however steadily they go on */
  let idleTimer: ReturnType<typeof setTimeout> | undefined;
  let capTimer: ReturnType<typeof setTimeout> | undefined;
  const clearTimers = () => {
    clearTimeout(idleTimer);
    clearTimeout(capTimer);
    idleTimer = capTimer = undefined;
  };

  /** Runs a sync off the main thread; the commits made before it began are on disk when it resolves */
  const runSync = (): Promise<void> => {
    owed = false;
    clearTimers();
    syncs++;
    const done = new Promise<void>((resolve, reject) => {
      syncEnvironment(root, (error) => {
        // A sync that failed made nothing durable: its commits are owed still, and the next sync takes them
        if (error) { owed = true; reject(error); } else resolve();
      });
    });
    // Every caller sharing it hears how it ended; a sync nobody waits on (a timer's) is caught where it is started
    running = done;
    const ended = () => { if (running === done) running = undefined; };
    done.then(ended, ended);
    return done;
  };

  /** A sync that covers every commit made so far: the one running when nothing was committed since, else a new one */
  const sync = (): Promise<void> => {
    if (closed) return Promise.resolve();
    if (!owed) return running ?? Promise.resolve();
    if (!running) return runSync();
    // A sync is running and began before the latest commits: one more follows it, however that one ends
    queued ??= running.then(() => {}, () => {}).then(() => { queued = undefined; return owed ? runSync() : undefined; });
    return queued;
  };

  /**
   * A sync a timer starts, which nobody waits on: a failure is not thrown at the process, and its commits stay owed
   * for the next commit, `durable()` or `close()` to sync
   */
  const syncInBackground = () => { sync().catch(() => {}); };

  const backend = createLmdbKVBackend(root, db, {
    committed() {
      owed = true;
      if (closed) return;
      // Neither timer keeps a process alive: closing syncs what is owed
      clearTimeout(idleTimer);
      idleTimer = setTimeout(syncInBackground, syncIdleMs);
      idleTimer.unref?.();
      if (!capTimer) {
        capTimer = setTimeout(syncInBackground, syncIntervalMs);
        capTimer.unref?.();
      }
    },
    synced() {
      owed = false;
      clearTimers();
    },
    durable: sync,
  });
  const refuseReadOnly = (what: string) => {
    if (readOnly) throw new Error(`The LMDB store at ${dir} is open read-only: it can't ${what}`);
  };
  // LMDB opened read-only has no write transaction to take, so a write is refused here, naming why
  const kv = createOrderedKV(readOnly ? { ...backend, write: () => refuseReadOnly('be written') as never } : backend);

  /** Writes the marker of a durable commit: a unit with no write wouldn't commit, nor so sync */
  const SYNCED = ['m', 'synced'] as const;


  return {
    kv,
    path: dir,
    readOnly,
    get syncs() { return syncs; },
    // One line per slot, under a header
    readers: () => root.readerList().trim().split('\n').slice(1).length,
    durable: sync,
    async copyTo(targetDir) {
      // LMDB writes data.mdb into a directory that already exists, and no lock.mdb: it makes one when opened
      fs.mkdirSync(targetDir, { recursive: true });
      await root.backup(targetDir, COMPACT_COPIES);
    },
    replaceWith(source) {
      refuseReadOnly('be replaced');
      const from = openEnvironment(source, { readOnly: true });
      try {
        backend.write((w) => {
          w.clear();
          for (const { key, value } of from.db.getRange()) w.put(key as Uint8Array, value as Uint8Array);
        }, { durable: true });
      } finally {
        from.root.close();
      }
    },
    reset() {
      refuseReadOnly('be reset');
      kv.write((w) => {
        w.clear();
        w.put(SYNCED, Date.now());
      }, { durable: true });
    },
    close() {
      if (closed) return;
      closed = true;
      clearTimers();
      // What is owed is synced before the files close: closing doesn't sync on its own
      if (owed && !readOnly) kv.write((w) => w.put(SYNCED, Date.now()), { durable: true });
      root.close();
    },
  };
}

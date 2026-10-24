import * as fs from 'node:fs';
import type { PersistenceSink, PersistenceErrorStats } from '../runtime.ts';
import type { EarsAdmin } from '../engine.ts';
import { openEnvAt, closeEnv, isAlreadyClosed, type LmdbDbs } from './envs.ts';
import { makeLmdbAdapter, type WriteFailure } from './adapter.ts';
import { hydrate } from './hydrate.ts';
import { LmdbReader } from './reader.ts';

/** An app's LMDB store: one environment, and the sink that writes the engine's changes to it */
export interface LmdbStore {
  /**
   * The engine's persistence: create the engine with it (`createEarsEngine({ persistence: store.sink })`).
   * It stays the same object when the store reopens, and drops writes while the store is closed.
   */
  readonly sink: Required<PersistenceSink>;
  /** The database directory */
  readonly path: string;
  /** Whether the store was opened read-only: its sink then throws on every write */
  readonly readOnly: boolean;
  /** The open environment. Throws while the store is closed */
  readonly env: LmdbDbs;
  /** Whether the environment is open */
  isOpen(): boolean;
  /** Loads the database into the store's engine */
  hydrate(): Promise<void>;
  /** Direct reads of the environment, without hydrating it */
  read(): LmdbReader;
  /**
   * Writes a consistent copy of the database into `targetDir` (as `data.mdb`, which the directory is created for).
   * LMDB copies one read transaction's worth of data, so the copy is the database as of a single moment even
   * while the app goes on writing — unlike copying the files, which can catch a commit half-made.
   */
  copyTo(targetDir: string): Promise<void>;
  /**
   * Flushes pending writes and closes the environment. Returns the failed writes of the environments it closed,
   * the final flush's included (none when the store was already closed)
   */
  close(): PersistenceErrorStats;
  /** Opens the environment again (after its files were replaced, say), closing it first if open */
  reopen(): void;
  /**
   * Closes the store, deletes its files and opens it empty. Doesn't touch the engine's memory. Writes made
   * while it's closed for the reset (the engine's own, from systems still running) are kept and written once
   * it's open again, so a reset loses nothing written after it started.
   */
  reset(): Promise<void>;
}

/** What an LMDB store is opened with */
export interface LmdbStoreOptions {
  /** The database directory */
  path: string;
  /**
   * The engine the store persists, which hydration loads into. A function, because the engine is created with the
   * store's sink.
   */
  engine: () => EarsAdmin;
  /** Opens existing environments without writing to them (tools reading a copy, or a running app's files) */
  readOnly?: boolean;
  /** Where the store's lines go (hydration counts, relations it skipped, errors closing); `console.log` by default */
  log?: (message: string) => void;
  /**
   * Told when a write is dropped, with the row it was for.
   *
   * A port rather than a log line: `@apackdev/ears` is below `@apackdev/sdk`, so the engine reports the fact and
   * the app decides what to do with it. Until this existed the only signal was `console.error`, and a
   * production app wrote 3,187 of them that nobody read.
   */
  onWriteFailure?: (failure: WriteFailure) => void;
}

const NO_ERRORS: PersistenceErrorStats = { errorCount: 0, lastError: null };

/**
 * Opens the LMDB environment at `path` and returns the store. Nothing reaches the engine until the caller creates
 * it with `store.sink` and hydrates.
 */
export function openLmdbStore({ path, engine, readOnly = false, log = console.log, onWriteFailure }: LmdbStoreOptions): LmdbStore {
  let env: LmdbDbs | null = null;
  let current: Required<PersistenceSink> | null = null;
  /** Failed writes of the environments closed so far, which `close()` reports with its own */
  let carried: PersistenceErrorStats = { errorCount: 0, lastError: null };
  /** Writes made while a reset has the store closed, written when it opens again */
  let heldForReset: Array<(sink: Required<PersistenceSink>) => void> | null = null;
  /**
   * Why opening the environments again failed, while it has: the store keeps no files to write to, so a write
   * can't be kept and throws instead of being dropped
   */
  let openFailure: Error | null = null;

  function open() {
    env = openEnvAt(path, { readOnly });
    openFailure = null;
    current = makeLmdbAdapter(env, onWriteFailure);
  }

  /** Closes the open environment and returns its failed writes, the final flush's included */
  function closeEnvs(): PersistenceErrorStats {
    const [closingEnv, closingSink] = [env, current];
    env = null;
    current = null;
    if (!closingEnv) return NO_ERRORS;
    const report = (error: unknown) => {
      if (!isAlreadyClosed(error)) log(`[LMDB] Closing ${path} met an error: ${(error as Error).message}`);
    };
    // The environments close even if the final flush throws
    try {
      closingSink?.close?.();
    } catch (error) {
      report(error);
    } finally {
      try {
        closeEnv(closingEnv);
        log('[LMDB] Environment closed successfully');
      } catch (error) {
        report(error);
      }
    }
    return closingSink?.getErrorStats?.() ?? NO_ERRORS;
  }

  /** The failed writes of every environment closed since the last report */
  function close(): PersistenceErrorStats {
    const closed = closeEnvs();
    const reported: PersistenceErrorStats = {
      errorCount: carried.errorCount + closed.errorCount,
      lastError: closed.lastError ?? carried.lastError,
    };
    carried = { errorCount: 0, lastError: null };
    return reported;
  }

  /**
   * Opens the environments again, after `closeEnvs`. A failure leaves the store with nowhere to write, which every
   * later write reports (`write`), and is thrown to the caller that asked for the reopen.
   */
  function openAgain(): void {
    try {
      open();
    } catch (error) {
      openFailure = error instanceof Error ? error : new Error(String(error));
      throw error;
    }
  }

  /** Keeps the failed writes of an environment closed on the way to opening another, for the next report */
  function carry(stats: PersistenceErrorStats): void {
    carried = { errorCount: carried.errorCount + stats.errorCount, lastError: stats.lastError ?? carried.lastError };
  }

  function openEnv(): LmdbDbs {
    if (!env) throw new Error('The LMDB store is closed');
    return env;
  }

  // Forwards to the current environments' sinks, so the engine keeps one sink across reopens. A closed store
  // drops writes, except during a reset, which holds them for the new files, and one whose files failed to open
  // again, where a write throws (`write`) rather than vanishing.
  const forward = (fn: (sink: Required<PersistenceSink>) => void) => {
    if (current) fn(current);
    else heldForReset?.push(fn);
  };
  const write = (fn: (sink: Required<PersistenceSink>) => void) => {
    if (readOnly) throw new Error(`The LMDB store at ${path} is open read-only`);
    if (openFailure) {
      throw new Error(
        `The LMDB store at ${path} is closed: opening it again failed (${openFailure.message}). `
        + 'Nothing can be saved until the app is started again.',
        { cause: openFailure },
      );
    }
    forward(fn);
  };
  const sink: Required<PersistenceSink> = {
    onCreateEntity: (...args) => write((s) => s.onCreateEntity(...args)),
    onDestroyEntity: (...args) => write((s) => s.onDestroyEntity(...args)),
    onDropAttr: (...args) => write((s) => s.onDropAttr(...args)),
    onPutAttr: (...args) => write((s) => s.onPutAttr(...args)),
    onAddRelation: (...args) => write((s) => s.onAddRelation(...args)),
    onUpdateRelation: (...args) => write((s) => s.onUpdateRelation(...args)),
    onRemoveRelation: (...args) => write((s) => s.onRemoveRelation(...args)),
    onUnitStart: () => write((s) => s.onUnitStart()),
    onUnitEnd: () => write((s) => s.onUnitEnd()),
    getErrorStats: () => current?.getErrorStats() ?? { errorCount: 0, lastError: null },
    close,
  };

  open();

  return {
    sink,
    path,
    readOnly,
    get env() { return openEnv(); },
    isOpen: () => env !== null,
    hydrate: () => hydrate({ engine: engine(), env: openEnv(), log }),
    read: () => new LmdbReader(openEnv()),
    async copyTo(targetDir) {
      // LMDB writes data.mdb into a directory that already exists, and no lock.mdb: it makes one when opened
      fs.mkdirSync(targetDir, { recursive: true });
      await openEnv().root.backup(targetDir, false);
    },
    close,
    reopen() {
      carry(closeEnvs());
      openAgain();
    },
    async reset() {
      if (readOnly) throw new Error(`The LMDB store at ${path} is open read-only`);
      heldForReset = [];
      try {
        carry(closeEnvs());
        // Let LMDB release the files before deleting them
        await new Promise((resolve) => setTimeout(resolve, 100));
        fs.rmSync(path, { recursive: true, force: true });
        openAgain();
      } finally {
        const held = heldForReset;
        heldForReset = null;
        if (current) for (const fn of held) fn(current);
      }
    },
  };
}

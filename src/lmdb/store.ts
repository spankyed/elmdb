import * as fs from 'node:fs';
import type { PersistenceSink, PersistenceErrorStats } from '../runtime.ts';
import { EARS } from '../entities.ts';
import type { EarsAdmin } from '../engine.ts';
import type { Partition, PartitionPolicy } from '../persistence/policy.ts';
import { makeShardedPersistence, type ShardedPersistence } from '../persistence/sharded-router.ts';
import { openShardedEnvs, closeShardedEnvs, deleteLmdbDirectories, type LmdbDbs, type LmdbPaths } from './envs.ts';
import { makeLmdbAdapter, type WriteFailure } from './adapter.ts';
import { hydrateSharded } from './hydrate.ts';
import { LmdbQuery } from './query.ts';

/** An app's LMDB store: one environment per partition, and the sink that writes the engine's changes to them */
export interface LmdbStore {
  /**
   * The engine's persistence: create the engine with it (`createEarsEngine({ persistence: store.sink })`).
   * It stays the same object when the store reopens, and drops writes while the store is closed.
   */
  readonly sink: ShardedPersistence;
  /** The partition policy the store was opened with */
  readonly policy: PartitionPolicy;
  /** Each partition's database directory */
  readonly paths: Readonly<LmdbPaths>;
  /** Whether the store was opened read-only: its sink then throws on every write */
  readonly readOnly: boolean;
  /** Each partition's open environment. Throws while the store is closed */
  readonly envs: Readonly<Record<Partition, LmdbDbs>>;
  /** Whether the environments are open */
  isOpen(): boolean;
  /** Loads the partitions the policy hydrates (and the volatile one with `includeVolatile`) into the store's engine */
  hydrate(options?: { includeVolatile?: boolean }): Promise<void>;
  /** Direct reads of a partition's environment, without hydrating it */
  query(partition: Partition): LmdbQuery;
  /**
   * Writes a consistent copy of `partition` into `targetDir` (as `data.mdb`, which the directory is created for).
   * LMDB copies one read transaction's worth of data, so the copy is the database as of a single moment even
   * while the app goes on writing — unlike copying the files, which can catch a commit half-made.
   */
  copyTo(partition: Partition, targetDir: string): Promise<void>;
  /**
   * Flushes pending writes and closes the environments. Returns the failed writes of the environments it closed,
   * the final flush's included (none when the store was already closed)
   */
  close(): PersistenceErrorStats;
  /** Opens the environments again (after their files were replaced, say), closing them first if open */
  reopen(): void;
  /**
   * Closes the store, deletes its files and opens it empty. Doesn't touch the engine's memory. Writes made
   * while it's closed for the reset (the engine's own, from systems still running) are kept and written once
   * it's open again, so a reset loses nothing written after it started.
   */
  reset(): Promise<void>;
}

const ignoresClosed = (error: unknown) =>
  error instanceof Error && (error.message.includes('Dbi is not open') || error.message.includes('already been closed'));

/** What an LMDB store is opened with */
export interface LmdbStoreOptions {
  /** Each partition's database directory */
  paths: LmdbPaths;
  /** Which partition each entity and relation lives in */
  policy: PartitionPolicy;
  /**
   * The engine the store persists: hydration loads into it, and the sink reads a relation's details from it.
   * A function, because the engine is created with the store's sink.
   */
  engine: () => EarsAdmin;
  /** Opens existing environments without writing to them (tools reading a copy, or a running app's files) */
  readOnly?: boolean;
  /** Where the store's progress lines go (hydration counts, closing); `console.log` by default */
  log?: (message: string) => void;
  /**
   * Told when a write is dropped, with the row it was for.
   *
   * A port rather than a log line: `@apack/ears` is below `@apack/sdk`, so the engine reports the fact and
   * the app decides what to do with it. Until this existed the only signal was `console.error`, and a
   * production app wrote 3,187 of them that nobody read.
   */
  onWriteFailure?: (failure: WriteFailure) => void;
}

const NO_ERRORS: PersistenceErrorStats = { errorCount: 0, lastError: null };

/**
 * Opens the LMDB environments at `paths` and returns the store. Nothing reaches the engine until the
 * caller creates it with `store.sink` and hydrates.
 */
export function openLmdbStore({ paths, policy, engine, readOnly = false, log = console.log, onWriteFailure }: LmdbStoreOptions): LmdbStore {
  const relationDetails = (relId: EARS.EntityId) =>
    engine().getAttr(relId, EARS.AttrKind.RelationDetails) as EARS.RelationDetail | null;
  let envs: Record<Partition, LmdbDbs> | null = null;
  let current: ShardedPersistence | null = null;
  /** Failed writes of the environments closed so far, which `close()` reports with its own */
  let carried: PersistenceErrorStats = { errorCount: 0, lastError: null };
  /** Writes made while a reset has the store closed, written when it opens again */
  let heldForReset: Array<(sink: ShardedPersistence) => void> | null = null;
  /**
   * Why opening the environments again failed, while it has: the store keeps no files to write to, so a write
   * can't be kept and throws instead of being dropped
   */
  let openFailure: Error | null = null;

  function open() {
    envs = openShardedEnvs(paths, { readOnly });
    openFailure = null;
    current = makeShardedPersistence(policy, {
      primary: makeLmdbAdapter(envs.primary, onWriteFailure),
      volatileBackup: makeLmdbAdapter(envs.volatileBackup, onWriteFailure),
    }, relationDetails);
  }

  /** Closes the open environments and returns their failed writes, the final flush's included */
  function closeEnvs(): PersistenceErrorStats {
    const [closingEnvs, closingSink] = [envs, current];
    envs = null;
    current = null;
    if (!closingEnvs) return NO_ERRORS;
    const warn = (error: unknown) => {
      if (!ignoresClosed(error)) console.warn('[Persistence] Non-critical close error:', (error as Error).message);
    };
    // The environments close even if the final flush throws
    try {
      closingSink?.close?.();
    } catch (error) {
      warn(error);
    } finally {
      try {
        closeShardedEnvs(closingEnvs, log);
      } catch (error) {
        warn(error);
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

  function openEnvs(): Record<Partition, LmdbDbs> {
    if (!envs) throw new Error('The LMDB store is closed');
    return envs;
  }

  // Forwards to the current environments' sinks, so the engine keeps one sink across reopens. A closed store
  // drops writes, except during a reset, which holds them for the new files, and one whose files failed to open
  // again, where a write throws (`write`) rather than vanishing.
  const forward = (fn: (sink: ShardedPersistence) => void) => {
    if (current) fn(current);
    else heldForReset?.push(fn);
  };
  const write = (fn: (sink: ShardedPersistence) => void) => {
    if (readOnly) throw new Error(`The LMDB store at ${paths.primary} is open read-only`);
    if (openFailure) {
      throw new Error(
        `The LMDB store at ${paths.primary} is closed: opening it again failed (${openFailure.message}). `
        + 'Nothing can be saved until the app is started again.',
        { cause: openFailure },
      );
    }
    forward(fn);
  };
  const sink: ShardedPersistence = {
    onCreateEntity: (...args) => write((s) => s.onCreateEntity(...args)),
    onDestroyEntity: (...args) => write((s) => s.onDestroyEntity(...args)),
    onDropAttr: (...args) => write((s) => s.onDropAttr(...args)),
    onPutAttrArray: (...args) => write((s) => s.onPutAttrArray(...args)),
    onAddRelation: (...args) => write((s) => s.onAddRelation(...args)),
    onUpdateRelation: (...args) => write((s) => s.onUpdateRelation(...args)),
    onRemoveRelation: (...args) => write((s) => s.onRemoveRelation(...args)),
    // Hydration filling the sink's relation cache, not a write
    hydrateRelationMetadata: (...args) => forward((s) => s.hydrateRelationMetadata(...args)),
    getRelMeta: () => current?.getRelMeta() ?? new Map(),
    getErrorStats: () => current?.getErrorStats?.() ?? { errorCount: 0, lastError: null },
    close,
  } satisfies Required<PersistenceSink> & ShardedPersistence;

  open();

  return {
    sink,
    policy,
    paths,
    readOnly,
    get envs() { return openEnvs(); },
    isOpen: () => envs !== null,
    hydrate: ({ includeVolatile = false } = {}) =>
      hydrateSharded({ engine: engine(), envs: openEnvs(), policy, includeVolatile, shardedPersistence: sink, log }),
    query: (partition) => new LmdbQuery(openEnvs()[partition]),
    async copyTo(partition, targetDir) {
      // LMDB writes data.mdb into a directory that already exists, and no lock.mdb: it makes one when opened
      fs.mkdirSync(targetDir, { recursive: true });
      await openEnvs()[partition].root.backup(targetDir, false);
    },
    close,
    reopen() {
      carry(closeEnvs());
      openAgain();
    },
    async reset() {
      if (readOnly) throw new Error(`The LMDB store at ${paths.primary} is open read-only`);
      heldForReset = [];
      try {
        carry(closeEnvs());
        // Let LMDB release the files before deleting them
        await new Promise((resolve) => setTimeout(resolve, 100));
        deleteLmdbDirectories(paths);
        openAgain();
      } finally {
        const held = heldForReset;
        heldForReset = null;
        if (current) for (const fn of held) fn(current);
      }
    },
  };
}

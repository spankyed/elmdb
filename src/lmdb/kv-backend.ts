/// <reference types="node" />
import { TransactionFlags, type Database, type RootDatabase, type Transaction } from 'lmdb';
import type { ByteReader, ByteWriter, KvBackend } from '../kv.ts';

/**
 * How a unit commits: synchronously and abortably, without waiting for the disk (`NO_SYNC_FLUSH`). The commit is
 * visible at once and survives the process crashing; what makes it durable against the machine failing is a sync,
 * which is the store's to schedule. A durable unit commits with the sync (`COMMIT_DURABLE`).
 */
const COMMIT = TransactionFlags.ABORTABLE | TransactionFlags.SYNCHRONOUS_COMMIT | TransactionFlags.NO_SYNC_FLUSH;
const COMMIT_DURABLE = TransactionFlags.ABORTABLE | TransactionFlags.SYNCHRONOUS_COMMIT;

/**
 * The two lmdb-js members this backend uses that its types leave out: `sync`, its environment's `mdb_env_sync` run off
 * the main thread, calling back once it finishes; and `getBinaryFast`'s read options, without which it reads in the
 * thread's latest read transaction rather than the snapshot given
 */
interface LmdbUntyped {
  sync(done: (error?: Error) => void): void;
  getBinaryFast(key: Uint8Array, options?: { transaction?: Transaction }): Uint8Array | undefined;
}

/** Syncs the environment off the main thread: every commit before the call is on disk once `done` is called without an error */
export function syncEnvironment(root: RootDatabase, done: (error?: Error) => void): void {
  (root as unknown as LmdbUntyped).sync(done);
}

/** What the backend tells its store, and asks of it */
export interface LmdbKVHooks {
  /** A unit committed without a sync: one is owed */
  committed(): void;
  /** A unit committed with a sync: every commit before it is on disk too */
  synced(): void;
  /** Resolves once every unit committed before the call is on disk */
  durable(): Promise<void>;
}

/**
 * The bytes of one database, read inside `transaction` (none: the open write unit, or the latest committed). A value
 * `get` gives back is lmdb-js's buffer for the next read, unless the seam copies it (`ByteReader`)
 */
function bytes(db: Database<Uint8Array, Uint8Array>, transaction?: Transaction): ByteReader {
  const options = transaction && { transaction };
  const fast = db as unknown as LmdbUntyped;
  return {
    get: (key) => fast.getBinaryFast(key, options),
    scan(low, high, reverse, values, visit) {
      // A reverse range starts at the high bound, which LMDB includes and the seam doesn't, and ends at the low one,
      // which LMDB excludes and the seam includes: the walk below skips the one and visits the other last
      // An empty bound is no bound: LMDB holds no empty key
      const lowBound = low.length > 0 ? low : undefined;
      const options_ = { start: reverse ? high : lowBound, end: reverse ? lowBound : high, reverse, ...options };
      // for…of closes the cursor however the loop ends — a false visit, a throw — which frees its reader slot
      if (values) {
        for (const { key, value } of db.getRange(options_ as Parameters<typeof db.getRange>[0])) {
          if (reverse && high && Buffer.compare(key as Buffer, high) === 0) continue;
          if (!visit(key as Uint8Array, value)) return;
        }
      } else {
        for (const key of db.getKeys(options_ as Parameters<typeof db.getKeys>[0])) {
          if (reverse && high && Buffer.compare(key as Buffer, high) === 0) continue;
          if (!visit(key as Uint8Array, undefined)) return;
        }
      }
      if (reverse && lowBound) {
        // `end` excluded `low` itself: visit it last, as a forward range visits it first
        const own = db.get(low as Buffer, options);
        if (own !== undefined) visit(low, values ? own : undefined);
      }
    },
  };
}

/**
 * The seam's backend over one LMDB database holding binary keys and values: `root` is its environment, which
 * transactions are taken on. A read is one read transaction, released however `fn` ends; a write is one
 * `transactionSync` (`COMMIT`), whose reads see its writes.
 */
export function createLmdbKVBackend(root: RootDatabase, db: Database<Uint8Array, Uint8Array>, hooks: LmdbKVHooks): KvBackend {
  return {
    read(fn) {
      const transaction = root.useReadTransaction();
      try {
        return fn(bytes(db, transaction));
      } finally {
        transaction.done();
      }
    },
    write(fn, options) {
      let result!: ReturnType<typeof fn>;
      let wrote = false;
      // A block body: a callback returning a promise would make the transaction asynchronous
      root.transactionSync(() => {
        const writer: ByteWriter = {
          ...bytes(db),
          put: (key, value) => { wrote = true; db.putSync(key as Buffer, value as Buffer); },
          del: (key) => { wrote = true; db.removeSync(key as Buffer); },
          // mdb_drop, emptying the database in this transaction: inside one, lmdb-js runs clearSync's in it
          clear: () => { wrote = true; db.clearSync(); },
        };
        result = fn(writer);
      }, options?.durable ? COMMIT_DURABLE : COMMIT);
      if (options?.durable) hooks.synced();
      else if (wrote) hooks.committed();
      return result;
    },
    durable: () => hooks.durable(),
  };
}

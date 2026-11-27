import type {
  BatchResult,
  CheckReport,
  DatabaseStats,
  DumpReport,
  IndexStatus,
  OpenReport,
  RelationRow,
  Schema,
  SchemaChange,
  TransactOptions,
} from 'elmdb';
import { createBatchWriter, encodeBatch, isThenable, type BatchWriter, type LinkOptions } from './batch.ts';
import type { CommitDelta, EarsDatabase, EdgeDirection } from './database.ts';
import { EarsError, hasKnownErrorCode, toEarsError } from './errors.ts';
import type { EarsOperationObserver } from './observer.ts';
import { createQueryRoot, type EntityRow, type QueryRoot } from './query.ts';
import { createSystemClock, WatchPump, type EarsClock, type WatchController } from './watch.ts';

/**
 * Synchronous hook invoked inside a native LMDB nested savepoint before a commit finalizes.
 *
 * - Throw any error to abort the entire commit atomically (`HOOK_ABORTED`).
 * - Return `void`/`undefined` to allow the commit as-is.
 * - Return a `(batch: BatchWriter) => void` callback or an encoded `Uint8Array` to apply derived writes
 *   in the same savepoint cascade.
 */
export type BeforeCommitHook = (
  delta: CommitDelta,
) => void | Uint8Array | ((batch: BatchWriter) => void);

/** Options for `EarsEngine.getEdges`. */
export interface GetEdgesOptions {
  readonly relKind?: string;
  readonly dir?: EdgeDirection;
}

/** Options for `createEarsEngine`. */
export interface CreateEarsEngineOptions {
  readonly database: EarsDatabase;
  readonly clock?: EarsClock;
  /** Optional fallback handler for errors thrown inside watch callbacks or reruns. */
  readonly onWatchError?: (error: EarsError) => void;
  /** Optional synchronous operation observer (zero overhead when omitted). */
  readonly observer?: EarsOperationObserver;
}

/** Administrative, schema evolution, and maintenance operations on an `EarsEngine`. */
export interface EarsAdmin {
  readonly path: string;
  readonly readOnly: boolean;
  openReport(): OpenReport;
  acknowledgeRestore(): void;
  setSchema(schema: Schema): SchemaChange;
  getSchema(): Schema;
  renameType(oldType: string, newType: string, rewriteIds?: boolean): number;
  renameField(typeName: string, oldField: string, newField: string): number;
  renameRelation(oldRel: string, newRel: string): number;
  purgeArchived(typeName: string): number;
  driveBackgroundWork(maxEntities?: number): boolean;
  flush(): void;
  flushAsync(): Promise<void>;
  check(indexName?: string): CheckReport;
  checkAsync(indexName?: string): Promise<CheckReport>;
  rebuild(indexName?: string): void;
  rebuildAsync(indexName?: string): Promise<void>;
  stats(): DatabaseStats;
  indexStatus(): IndexStatus[];
  backup(targetDir: string, overwriteBackup?: boolean): void;
  backupAsync(targetDir: string, overwriteBackup?: boolean): Promise<void>;
  dump(targetFile: string): DumpReport;
  close(): void;
}

/**
 * Primary instance-scoped EARS v2 engine handle wrapping an `EarsDatabase`.
 *
 * Holds zero module-level state: every write, query, hook, and watch is scoped strictly to this instance.
 */
export interface EarsEngine {
  readonly query: QueryRoot;
  readonly watches: WatchController;
  readonly admin: EarsAdmin;

  get(id: string): EntityRow | null;
  getEdges(id: string, opts?: GetEdgesOptions): RelationRow[];

  create(typeName: string, fields: Record<string, unknown>, externalId?: string): string;
  patch(id: string, fields: Record<string, unknown>): BatchResult;
  unset(id: string, fields: readonly string[]): BatchResult;
  trash(id: string): BatchResult;
  restore(id: string): BatchResult;
  destroy(id: string): BatchResult;
  link(sourceId: string, relKind: string, targetId: string, opts?: LinkOptions): BatchResult;
  unlink(sourceId: string, relKind?: string, targetId?: string): BatchResult;
  setEmbeddings(indexName: string, id: string, vectors: ReadonlyArray<ReadonlyArray<number>>): void;

  /** Executes a synchronous multi-operation atomic batch in a single `elmdb-wire` FFI crossing. */
  batch(fn: (batch: BatchWriter) => void): BatchResult;
  /** Executes a pre-encoded `elmdb-wire` binary batch directly. */
  executeWireBatch(payload: Uint8Array): BatchResult;

  /**
   * Executes a strictly-synchronous callback inside a write transaction (or native LMDB savepoint when nested).
   *
   * - Reads (`get`, `getEdges`, `query`, `getCounter`) and writes see all uncommitted writes immediately (RYOW).
   * - Throwing rolls back this transaction (or nested savepoint only, preserving outer writes).
   * - Returning a Promise or thenable aborts immediately with `EarsError('ASYNC_CALLBACK_FORBIDDEN')`.
   * - `beforeCommit` hooks run once on the outermost commit unless `{ runBeforeCommit: false }`.
   */
  transact<T>(fn: () => T, opts?: TransactOptions): T;
  /** Returns the pending commit sequence while inside `transact()` or `beforeCommit`, or `null` outside a write txn. */
  pendingCommitSeq(): number | null;
  /** Atomically increments and returns the named persistent counter (`1, 2, 3, ...`), participating in `transact()`. */
  nextCounter(name: string): number;
  /** Reads the current value of the named persistent counter (`0` if unset), with RYOW inside `transact()`. */
  getCounter(name: string): number;
  /** Sets the named persistent counter (e.g. preserving imported high-water marks), participating in `transact()`. */
  setCounter(name: string, value: number): void;

  /**
   * Registers a synchronous engine-native `beforeCommit` hook.
   * Pass `null` for `typeNames` to subscribe to all entity/edge mutations.
   * Returns an idempotent unsubscribe function.
   */
  beforeCommit(typeNames: readonly string[] | null, callback: BeforeCommitHook): () => void;
}

/** Creates an instance-scoped EARS v2 engine over an open `EarsDatabase`. */
export function createEarsEngine(options: CreateEarsEngineOptions): EarsEngine {
  const db = options.database;
  const clock = options.clock ?? createSystemClock();
  const observer = options.observer;
  let txnDepth = 0;
  let hookDepth = 0;
  let outerTxnMutated = false;
  const isInWriteTxn = (): boolean => txnDepth > 0 || hookDepth > 0;
  const pump = new WatchPump(db, clock, options.onWatchError, observer);
  const query = createQueryRoot(db, pump, observer, isInWriteTxn);

  const runBatchPayload = (payload: Uint8Array, ops = 1): BatchResult => {
    const finish = observer?.start({ kind: 'write', ops, watch: false, inUnit: isInWriteTxn() });
    let result: BatchResult;
    try {
      result = db.executeWireBatch(payload);
    } catch (error) {
      const earsErr = toEarsError(error);
      finish?.({
        outcome: 'error',
        requestBytes: payload.byteLength,
        responseBytes: 0,
        errorCode: earsErr.code,
      });
      throw earsErr;
    }
    if (isInWriteTxn()) {
      outerTxnMutated = true;
    }
    finish?.({
      outcome: 'ok',
      sequence: result.commitSeq,
      requestBytes: payload.byteLength,
      responseBytes: 0,
    });
    if (!isInWriteTxn()) {
      pump.notifyMutation();
    }
    return result;
  };

  const batch = (fn: (b: BatchWriter) => void): BatchResult => {
    const writer = createBatchWriter();
    let returned: unknown;
    try {
      returned = fn(writer);
    } catch (error) {
      throw toEarsError(error, 'INVALID_ARGUMENT');
    }
    if (isThenable(returned)) {
      throw new EarsError(
        'ASYNC_CALLBACK_FORBIDDEN',
        'Batch callbacks must be synchronous (Promises and async callbacks are forbidden)',
      );
    }
    return runBatchPayload(writer.encode(), writer.length);
  };

  const admin: EarsAdmin = {
    get path(): string {
      return db.path;
    },
    get readOnly(): boolean {
      return db.readOnly;
    },
    openReport(): OpenReport {
      try {
        return db.openReport();
      } catch (error) {
        throw toEarsError(error);
      }
    },
    acknowledgeRestore(): void {
      try {
        db.acknowledgeRestore();
      } catch (error) {
        throw toEarsError(error);
      }
    },
    setSchema(schema: Schema): SchemaChange {
      let change: SchemaChange;
      try {
        change = db.setSchema(schema);
      } catch (error) {
        throw toEarsError(error);
      }
      pump.notifyMutation();
      return change;
    },
    getSchema(): Schema {
      try {
        return db.getSchema();
      } catch (error) {
        throw toEarsError(error);
      }
    },
    renameType(oldType: string, newType: string, rewriteIds?: boolean): number {
      let count: number;
      try {
        count = db.renameType(oldType, newType, rewriteIds);
      } catch (error) {
        throw toEarsError(error);
      }
      pump.notifyMutation();
      return count;
    },
    renameField(typeName: string, oldField: string, newField: string): number {
      let count: number;
      try {
        count = db.renameField(typeName, oldField, newField);
      } catch (error) {
        throw toEarsError(error);
      }
      pump.notifyMutation();
      return count;
    },
    renameRelation(oldRel: string, newRel: string): number {
      let count: number;
      try {
        count = db.renameRelation(oldRel, newRel);
      } catch (error) {
        throw toEarsError(error);
      }
      pump.notifyMutation();
      return count;
    },
    purgeArchived(typeName: string): number {
      let count: number;
      try {
        count = db.purgeArchived(typeName);
      } catch (error) {
        throw toEarsError(error);
      }
      pump.notifyMutation();
      return count;
    },
    driveBackgroundWork(maxEntities?: number): boolean {
      let more: boolean;
      try {
        more = db.driveBackgroundWork(maxEntities);
      } catch (error) {
        throw toEarsError(error);
      }
      pump.notifyMutation();
      return more;
    },
    flush(): void {
      try {
        db.flush();
      } catch (error) {
        throw toEarsError(error);
      }
      pump.notifyMutation();
    },
    async flushAsync(): Promise<void> {
      try {
        await db.flushAsync();
      } catch (error) {
        throw toEarsError(error);
      }
      pump.notifyMutation();
    },
    check(indexName?: string): CheckReport {
      try {
        return db.check(indexName);
      } catch (error) {
        throw toEarsError(error);
      }
    },
    async checkAsync(indexName?: string): Promise<CheckReport> {
      try {
        return await db.checkAsync(indexName);
      } catch (error) {
        throw toEarsError(error);
      }
    },
    rebuild(indexName?: string): void {
      try {
        db.rebuild(indexName);
      } catch (error) {
        throw toEarsError(error);
      }
      pump.notifyMutation();
    },
    async rebuildAsync(indexName?: string): Promise<void> {
      try {
        await db.rebuildAsync(indexName);
      } catch (error) {
        throw toEarsError(error);
      }
      pump.notifyMutation();
    },
    stats(): DatabaseStats {
      try {
        return db.stats();
      } catch (error) {
        throw toEarsError(error);
      }
    },
    indexStatus(): IndexStatus[] {
      try {
        return db.indexStatus();
      } catch (error) {
        throw toEarsError(error);
      }
    },
    backup(targetDir: string, overwriteBackup?: boolean): void {
      try {
        db.backup(targetDir, overwriteBackup);
      } catch (error) {
        throw toEarsError(error);
      }
    },
    async backupAsync(targetDir: string, overwriteBackup?: boolean): Promise<void> {
      try {
        await db.backupAsync(targetDir, overwriteBackup);
      } catch (error) {
        throw toEarsError(error);
      }
    },
    dump(targetFile: string): DumpReport {
      try {
        return db.dump(targetFile);
      } catch (error) {
        throw toEarsError(error);
      }
    },
    close(): void {
      // Close the underlying database FIRST so rejecting `close()` inside `transact()`
      // never permanently stops `WatchPump` on an open database!
      try {
        db.close();
      } catch (error) {
        throw toEarsError(error);
      }
      pump.markClosed();
    },
  };

  return {
    query,
    watches: pump,
    admin,
    get(id: string): EntityRow | null {
      const finish = observer?.start({ kind: 'read', ops: 1, watch: false, inUnit: isInWriteTxn() });
      let row: EntityRow | null;
      try {
        row = db.get(id);
      } catch (error) {
        const earsErr = toEarsError(error);
        finish?.({ outcome: 'error', requestBytes: 0, responseBytes: 0, errorCode: earsErr.code });
        throw earsErr;
      }
      finish?.({ outcome: 'ok', requestBytes: 0, responseBytes: 0 });
      return row;
    },
    getEdges(id: string, opts?: GetEdgesOptions): RelationRow[] {
      const finish = observer?.start({ kind: 'read', ops: 1, watch: false, inUnit: isInWriteTxn() });
      let edges: RelationRow[];
      try {
        edges = db.getEdges(id, opts?.relKind, opts?.dir);
      } catch (error) {
        const earsErr = toEarsError(error);
        finish?.({ outcome: 'error', requestBytes: 0, responseBytes: 0, errorCode: earsErr.code });
        throw earsErr;
      }
      finish?.({ outcome: 'ok', requestBytes: 0, responseBytes: 0 });
      return edges;
    },
    create(typeName: string, fields: Record<string, unknown>, externalId?: string): string {
      const res = batch((b) => {
        b.create(typeName, fields, externalId);
      });
      const created = res.createdIds[0];
      if (created === undefined) {
        throw new EarsError('INTERNAL', 'create batch committed without returning a created entity ID');
      }
      return created;
    },
    patch(id: string, fields: Record<string, unknown>): BatchResult {
      return batch((b) => {
        b.patch(id, fields);
      });
    },
    unset(id: string, fields: readonly string[]): BatchResult {
      return batch((b) => {
        b.unset(id, fields);
      });
    },
    trash(id: string): BatchResult {
      return batch((b) => {
        b.trash(id);
      });
    },
    restore(id: string): BatchResult {
      return batch((b) => {
        b.restore(id);
      });
    },
    destroy(id: string): BatchResult {
      return batch((b) => {
        b.destroy(id);
      });
    },
    link(sourceId: string, relKind: string, targetId: string, opts?: LinkOptions): BatchResult {
      return batch((b) => {
        b.link(sourceId, relKind, targetId, opts);
      });
    },
    unlink(sourceId: string, relKind?: string, targetId?: string): BatchResult {
      return batch((b) => {
        b.unlink(sourceId, relKind, targetId);
      });
    },
    setEmbeddings(indexName: string, id: string, vectors: ReadonlyArray<ReadonlyArray<number>>): void {
      const finish = observer?.start({ kind: 'write', ops: 1, watch: false, inUnit: isInWriteTxn() });
      try {
        db.setEmbeddings(
          indexName,
          id,
          vectors.map((v) => [...v]),
        );
      } catch (error) {
        const earsErr = toEarsError(error);
        finish?.({ outcome: 'error', requestBytes: 0, responseBytes: 0, errorCode: earsErr.code });
        throw earsErr;
      }
      finish?.({ outcome: 'ok', requestBytes: 0, responseBytes: 0 });
      pump.notifyMutation();
    },
    batch,
    executeWireBatch: (payload: Uint8Array): BatchResult => runBatchPayload(payload, 1),
    transact<T>(fn: () => T, opts?: TransactOptions): T {
      const isOuter = !isInWriteTxn();
      const finish = isOuter
        ? observer?.start({ kind: 'unit', ops: 1, watch: false, inUnit: false })
        : undefined;
      if (isOuter) outerTxnMutated = false;
      let commitSeq: number | undefined;
      txnDepth++;
      let value: T;
      try {
        value = db.transact(() => {
          const result = fn();
          if (isThenable(result)) {
            throw new EarsError(
              'ASYNC_CALLBACK_FORBIDDEN',
              'transact() callbacks must be synchronous (Promises and async callbacks are forbidden)',
            );
          }
          if (isOuter && finish !== undefined && outerTxnMutated) {
            const pending = db.pendingCommitSeq();
            if (pending !== null) commitSeq = pending;
          }
          return result;
        }, opts) as T;
      } catch (error) {
        txnDepth--;
        if (hasKnownErrorCode(error)) {
          const earsErr = toEarsError(error);
          finish?.({ outcome: 'aborted', requestBytes: 0, responseBytes: 0, errorCode: earsErr.code });
          throw earsErr;
        }
        finish?.({ outcome: 'aborted', requestBytes: 0, responseBytes: 0 });
        throw error;
      }
      txnDepth--;
      finish?.({
        outcome: 'ok',
        ...(commitSeq !== undefined && { sequence: commitSeq }),
        requestBytes: 0,
        responseBytes: 0,
      });
      if (isOuter) {
        pump.notifyMutation();
      }
      return value;
    },
    pendingCommitSeq(): number | null {
      try {
        return db.pendingCommitSeq();
      } catch (error) {
        throw toEarsError(error);
      }
    },
    nextCounter(name: string): number {
      const finish = observer?.start({ kind: 'write', ops: 1, watch: false, inUnit: isInWriteTxn() });
      let val: number;
      try {
        val = db.nextCounter(name);
      } catch (error) {
        const earsErr = toEarsError(error);
        finish?.({ outcome: 'error', requestBytes: 0, responseBytes: 0, errorCode: earsErr.code });
        throw earsErr;
      }
      if (isInWriteTxn()) outerTxnMutated = true;
      finish?.({ outcome: 'ok', requestBytes: 0, responseBytes: 0 });
      return val;
    },
    getCounter(name: string): number {
      const finish = observer?.start({ kind: 'read', ops: 1, watch: false, inUnit: isInWriteTxn() });
      let val: number;
      try {
        val = db.getCounter(name);
      } catch (error) {
        const earsErr = toEarsError(error);
        finish?.({ outcome: 'error', requestBytes: 0, responseBytes: 0, errorCode: earsErr.code });
        throw earsErr;
      }
      finish?.({ outcome: 'ok', requestBytes: 0, responseBytes: 0 });
      return val;
    },
    setCounter(name: string, value: number): void {
      const finish = observer?.start({ kind: 'write', ops: 1, watch: false, inUnit: isInWriteTxn() });
      try {
        db.setCounter(name, value);
      } catch (error) {
        const earsErr = toEarsError(error);
        finish?.({ outcome: 'error', requestBytes: 0, responseBytes: 0, errorCode: earsErr.code });
        throw earsErr;
      }
      if (isInWriteTxn()) outerTxnMutated = true;
      finish?.({ outcome: 'ok', requestBytes: 0, responseBytes: 0 });
    },
    beforeCommit(typeNames: readonly string[] | null, callback: BeforeCommitHook): () => void {
      let hookId: number;
      try {
        hookId = db.registerBeforeCommit(
          typeNames === null ? null : [...typeNames],
          (delta: CommitDelta): unknown => {
            hookDepth++;
            try {
              const ret: unknown = callback(delta);
              if (typeof ret === 'function') {
                return encodeBatch(ret as (b: BatchWriter) => void);
              }
              // Pass thenables and invalid return types straight to the native binding so it raises
              // exact `ASYNC_CALLBACK_FORBIDDEN` / `INVALID_ARGUMENT` error codes cleanly.
              return ret;
            } finally {
              hookDepth--;
            }
          },
        );
      } catch (error) {
        throw toEarsError(error);
      }
      let active = true;
      return () => {
        if (!active) return;
        active = false;
        try {
          db.unregisterBeforeCommit(hookId);
        } catch {
          // Best-effort if database was already closed.
        }
      };
    },
  };
}

import type { Schema } from './protocol/Schema.js'
import type { SchemaChange } from './protocol/SchemaChange.js'
import type { OpenReport } from './protocol/OpenReport.js'
import type { BatchResult } from './protocol/BatchResult.js'
import type { CheckReport } from './protocol/CheckReport.js'
import type { IndexStatus } from './protocol/IndexStatus.js'
import type { DatabaseStats } from './protocol/DatabaseStats.js'
import type { DumpReport } from './protocol/DumpReport.js'
import type { WatchTick } from './protocol/WatchTick.js'
import type { Row } from './protocol/Row.js'
import type { RelationRow } from './protocol/RelationRow.js'
import type { QuerySpec } from './protocol/QuerySpec.js'
import type { QueryAnswer } from './protocol/QueryAnswer.js'
export type { ErrorCode } from './protocol/ErrorCode.js'
export type { Schema, SchemaChange, OpenReport, BatchResult, CheckReport, IndexStatus, DatabaseStats, DumpReport, WatchTick, Row, RelationRow, QuerySpec, QueryAnswer }

/**
 * Which binding artifact the generated loader actually loaded: `'native'` for
 * a native addon, otherwise the `platformArchABI` of the WASI flavor. Every
 * flavor napi-rs can build is listed, because `NAPI_RS_NATIVE_LIBRARY_PATH`
 * can point the loader at a WASI artifact this package does not build itself.
 */
export declare const __napiBindingTarget: 'native' | 'wasm32-wasi' | 'wasm32-wasip1'

/** Handle to an open elmdb v2 database directory. */
export declare class Database {
  /** Opens an existing elmdb v2 database or creates a new one in `dir`. */
  static open(dir: string, options?: OpenOptions | undefined | null): Database
  /** Opens an elmdb v2 database asynchronously on Node's worker pool. */
  static openAsync(dir: string, options?: OpenOptions | undefined | null): Promise<Database>
  /** Loads a portable v2 JSON dump file into a brand-new database directory. */
  static load(dumpFile: string, targetDir: string): DumpReport
  get path(): string
  get readOnly(): boolean
  openReport(): OpenReport
  acknowledgeRestore(): void
  setSchema(schema: Schema): SchemaChange
  getSchema(): Schema
  renameType(oldType: string, newType: string, rewriteIds?: boolean | undefined | null): number
  renameField(typeName: string, oldField: string, newField: string): number
  renameRelation(oldRel: string, newRel: string): number
  /** Executes an atomic `elmdb-wire` binary batch directly (single FFI crossing, zero MsgPack/JSON). */
  executeWireBatch(payload: Uint8Array): BatchResult
  /**
   * Executes a strictly-synchronous callback inside a write transaction (or native LMDB savepoint when nested).
   *
   * - Reads (`get`, `getEdges`, `query`, `getCounter`) and writes (`executeWireBatch`, `nextCounter`, `setCounter`)
   *   see all uncommitted outer and inner writes immediately (RYOW).
   * - Throwing rolls back this transaction (or nested savepoint only, preserving outer writes).
   * - Returning a Promise or thenable aborts immediately with `ASYNC_CALLBACK_FORBIDDEN`.
   * - `beforeCommit` hooks run once on the outermost commit unless `{ runBeforeCommit: false }`.
   */
  transact(callback: () => unknown, options?: TransactOptions | undefined | null): unknown
  /** Returns the pending commit sequence while inside `transact()` or `beforeCommit`, or `null` outside a write txn. */
  pendingCommitSeq(): number | null
  /**
   * Atomically increments and returns the named persistent counter (`1, 2, 3, ...`).
   * Participates in the active `transact()` / `beforeCommit` savepoint and rolls back on abort.
   */
  nextCounter(name: string): number
  /** Reads the current value of the named persistent counter (`0` if unset), with RYOW inside `transact()`. */
  getCounter(name: string): number
  /**
   * Sets the named persistent counter (e.g. preserving imported high-water marks).
   * Participates in the active `transact()` / `beforeCommit` savepoint and rolls back on abort.
   */
  setCounter(name: string, value: number): void
  /** Sets primary vector embeddings for `extId` on `indexName`. */
  setEmbeddings(indexName: string, extId: string, vectors: Array<Array<number>>): void
  /** Permanently destroys all entities belonging to an archived type. */
  purgeArchived(typeName: string): number
  /**
   * Registers a synchronous same-thread JS `beforeCommit` hook.
   *
   * The callback receives `{ commitSeq, entities, edges }` inside a native LMDB nested savepoint:
   * - Full read-your-own-writes (`get`, `getEdges`, `query`, `getCounter`, `nextCounter`, `setCounter`,
   *   `pendingCommitSeq`, `executeWireBatch`, and nested `transact`) are available directly inside the hook.
   * - Throwing an error aborts the commit cleanly with `HOOK_ABORTED`.
   * - Returning a `Uint8Array`/`Buffer` containing an `elmdb-wire` batch applies those derived writes
   *   in the same savepoint cascade (up to `MAX_HOOK_ROUNDS`).
   */
  registerBeforeCommit(typeNames: Array<string> | undefined | null, callback: (delta: { commitSeq: number; entities: Array<{ id: string; typeName: string; op: 'create' | 'update' | 'trash' | 'restore' | 'destroy' }>; edges: Array<{ relKind: string; op: 'link' | 'unlink' | 'update'; sourceId: string; targetId: string }> }) => unknown): number
  unregisterBeforeCommit(hookId: number): boolean
  /** Reads one entity row by external ID with all `EntityRef`s resolved in a single pinned MVCC snapshot. */
  get(extId: string): Row | null
  /** Lists outgoing, incoming, or both-direction relations for `extId`. */
  getEdges(extId: string, relKind?: string | undefined | null, dir?: string | undefined | null): Array<RelationRow>
  /** Executes a `QuerySpec` synchronously on a pinned lock-free MVCC read snapshot. */
  query(spec: QuerySpec): QueryAnswer
  /** Executes a `QuerySpec` off-thread on Node's libuv worker pool. */
  queryAsync(spec: QuerySpec): Promise<QueryAnswer>
  registerWatch(spec: QuerySpec, policy?: WatchPolicyOptions | undefined | null, nowMs?: number | undefined | null): WatchRegistration
  rerunWatch(watchId: number, spec: QuerySpec, nowMs?: number | undefined | null, costMs?: number | undefined | null): WatchRerun
  unregisterWatch(watchId: number): boolean
  catchUpCrossProcess(nowMs?: number | undefined | null): Array<number>
  tickWatches(nowMs?: number | undefined | null): WatchTick
  nextDueWatch(nowMs?: number | undefined | null, elapsedTurnMs?: number | undefined | null): NextDueWatch
  beginWatchRun(watchId: number): boolean
  abortWatchRun(watchId: number, requeueDirty: boolean, nowMs?: number | undefined | null): boolean
  caughtUp(nowMs?: number | undefined | null, soon?: boolean | undefined | null): boolean
  watchesSettled(): boolean
  stopWatches(): void
  driveBackgroundWork(maxEntities?: number | undefined | null): boolean
  flush(): void
  flushAsync(): Promise<void>
  check(indexName?: string | undefined | null): CheckReport
  checkAsync(indexName?: string | undefined | null): Promise<CheckReport>
  rebuild(indexName?: string | undefined | null): void
  rebuildAsync(indexName?: string | undefined | null): Promise<void>
  stats(): DatabaseStats
  indexStatus(): Array<IndexStatus>
  backup(targetDir: string, overwriteBackup?: boolean | undefined | null): void
  backupAsync(targetDir: string, overwriteBackup?: boolean | undefined | null): Promise<void>
  dump(targetFile: string): DumpReport
  close(): void
}

/** Return value of `nextDueWatch`. */
export interface NextDueWatch {
  watchId?: number
  nextWakeDelayMs?: number
  settled: boolean
}

/** Options for `Database.open` / `Database.openAsync`. */
export interface OpenOptions {
  readOnly?: boolean
  schema?: Schema
  /** `"strict"` (default), `"grouped"`, or `"unsafe"` (tests). */
  durability?: string
  /** `"deferred"` (default, watermarked background builds) or `"eager"` (build inline on setSchema). */
  buildPolicy?: string
  mapSizeBytes?: number
  maxDbs?: number
  maxReaders?: number
  planCacheCapacity?: number
  watchTurnSliceMs?: number
}

/** Options for `Database.transact`. */
export interface TransactOptions {
  /** Whether registered `beforeCommit` hooks run when the outermost transaction commits (default `true`). */
  runBeforeCommit?: boolean
}

/** Watch throttling & delivery mode policy for `registerWatch`. */
export interface WatchPolicyOptions {
  /** `"declarative"` (default, 128-bit result dedup) or `"imperative"` (always deliver on invalidation). */
  mode?: string
  minGapMs?: number
  adaptiveMultiplier?: number
}

/** Return value of `registerWatch`. */
export interface WatchRegistration {
  watchId: number
  answer: QueryAnswer
}

/** Return value of `rerunWatch`. */
export interface WatchRerun {
  answer: QueryAnswer
  changed: boolean
}

import type {
  BatchResult,
  CheckReport,
  DatabaseStats,
  DumpReport,
  IndexStatus,
  NextDueWatch,
  OpenReport,
  QueryAnswer,
  QuerySpec,
  RelationRow,
  Row,
  Schema,
  SchemaChange,
  TransactOptions,
  WatchPolicyOptions,
  WatchRegistration,
  WatchRerun,
  WatchTick,
} from 'elmdb';

/** Graph traversal / edge listing direction. */
export type EdgeDirection = 'out' | 'in' | 'both';

/** Sort direction for ordered queries and scores. */
export type SortDirection = 'asc' | 'desc';

/** Multi-signal score fusion method. */
export type FusionMethod = 'rrf' | 'linear';

/** Durability policy for `openDatabase` / `openEngine`. */
export type DurabilityMode = 'strict' | 'grouped' | 'unsafe';

/** Index build policy for `openDatabase` / `openEngine`. */
export type BuildPolicy = 'deferred' | 'eager';

/** Entity mutation event delivered inside a synchronous `beforeCommit` hook. */
export interface EntityDeltaEvent {
  readonly op: 'create' | 'update' | 'trash' | 'restore' | 'destroy';
  readonly id: string;
  readonly typeName: string;
}

/** Relation mutation event delivered inside a synchronous `beforeCommit` hook. */
export interface EdgeDeltaEvent {
  readonly op: 'link' | 'unlink' | 'update';
  readonly relKind: string;
  readonly sourceId: string;
  readonly targetId: string;
}

/** Per-round primary delta payload passed to a `beforeCommit` hook inside a native LMDB savepoint. */
export interface CommitDelta {
  readonly commitSeq: number;
  readonly entities: readonly EntityDeltaEvent[];
  readonly edges: readonly EdgeDeltaEvent[];
}

/**
 * Pure port interface for an open elmdb v2 database handle.
 *
 * Kept free of native runtime imports so `@elmdb/ears` (root entrypoint) works in browser/renderer
 * contexts or over custom transports, while `elmdb.Database` satisfies this interface with zero casts.
 */
export interface EarsDatabase {
  readonly path: string;
  readonly readOnly: boolean;
  openReport(): OpenReport;
  acknowledgeRestore(): void;
  setSchema(schema: Schema): SchemaChange;
  getSchema(): Schema;
  renameType(oldType: string, newType: string, rewriteIds?: boolean | undefined | null): number;
  renameField(typeName: string, oldField: string, newField: string): number;
  renameRelation(oldRel: string, newRel: string): number;
  executeWireBatch(payload: Uint8Array): BatchResult;
  transact(callback: () => unknown, options?: TransactOptions | undefined | null): unknown;
  pendingCommitSeq(): number | null;
  nextCounter(name: string): number;
  getCounter(name: string): number;
  setCounter(name: string, value: number): void;
  setEmbeddings(indexName: string, extId: string, vectors: Array<Array<number>>): void;
  purgeArchived(typeName: string): number;
  registerBeforeCommit(
    typeNames: Array<string> | undefined | null,
    callback: (delta: CommitDelta) => unknown,
  ): number;
  unregisterBeforeCommit(hookId: number): boolean;
  get(extId: string): Row | null;
  getEdges(
    extId: string,
    relKind?: string | undefined | null,
    dir?: string | undefined | null,
  ): Array<RelationRow>;
  query(spec: QuerySpec): QueryAnswer;
  queryAsync(spec: QuerySpec): Promise<QueryAnswer>;
  registerWatch(
    spec: QuerySpec,
    policy?: WatchPolicyOptions | undefined | null,
    nowMs?: number | undefined | null,
  ): WatchRegistration;
  rerunWatch(
    watchId: number,
    spec: QuerySpec,
    nowMs?: number | undefined | null,
    costMs?: number | undefined | null,
  ): WatchRerun;
  unregisterWatch(watchId: number): boolean;
  catchUpCrossProcess(nowMs?: number | undefined | null): Array<number>;
  tickWatches(nowMs?: number | undefined | null): WatchTick;
  nextDueWatch(
    nowMs?: number | undefined | null,
    elapsedTurnMs?: number | undefined | null,
  ): NextDueWatch;
  beginWatchRun(watchId: number): boolean;
  abortWatchRun(watchId: number, requeueDirty: boolean, nowMs?: number | undefined | null): boolean;
  caughtUp(nowMs?: number | undefined | null, soon?: boolean | undefined | null): boolean;
  watchesSettled(): boolean;
  stopWatches(): void;
  driveBackgroundWork(maxEntities?: number | undefined | null): boolean;
  flush(): void;
  flushAsync(): Promise<void>;
  check(indexName?: string | undefined | null): CheckReport;
  checkAsync(indexName?: string | undefined | null): Promise<CheckReport>;
  rebuild(indexName?: string | undefined | null): void;
  rebuildAsync(indexName?: string | undefined | null): Promise<void>;
  stats(): DatabaseStats;
  indexStatus(): Array<IndexStatus>;
  backup(targetDir: string, overwriteBackup?: boolean | undefined | null): void;
  backupAsync(targetDir: string, overwriteBackup?: boolean | undefined | null): Promise<void>;
  dump(targetFile: string): DumpReport;
  close(): void;
}

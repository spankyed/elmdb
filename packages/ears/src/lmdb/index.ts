/**
 * `@elmdb/ears/database` — native elmdb v2 database openers (`openDatabase`, `openEngine`,
 * `loadDump`, `createTemporaryDatabases`).
 *
 * This is the sole entrypoint in `@elmdb/ears` that loads the native `elmdb` addon.
 *
 * @packageDocumentation
 */

export {
  DATABASE_FORMAT,
  createTemporaryDatabases,
  loadDump,
  openDatabase,
  openDatabaseAsync,
  openEngine,
  openEngineAsync,
  type DatabaseOptions,
  type OpenEngineOptions,
  type TemporaryDatabaseHandle,
  type TemporaryDatabases,
} from './database.ts';

export type {
  BuildPolicy,
  CommitDelta,
  DurabilityMode,
  EarsDatabase,
  EdgeDeltaEvent,
  EdgeDirection,
  EntityDeltaEvent,
} from '../database.ts';
export type {
  BeforeCommitHook,
  EarsAdmin,
  EarsEngine,
  GetEdgesOptions,
} from '../engine.ts';
export type { EarsError, EarsErrorCode } from '../errors.ts';
export type {
  EarsOperation,
  EarsOperationKind,
  EarsOperationObserver,
  EarsOperationResult,
} from '../observer.ts';
export type {
  DegreesOptions,
  EntityRow,
  FieldPredicate,
  FuseOptions,
  GraphPprResult,
  GraphTopoResult,
  HasLinkOptions,
  HitsOptions,
  HopsOptions,
  KeysetCursor,
  LeidenOptions,
  MinMaxResult,
  OrderByOptions,
  PprOptions,
  QueryBuilder,
  QueryRoot,
  RecencyOptions,
  SearchOptions,
  ShortestPathOptions,
  SimilarityOptions,
  SourceOptions,
  WatchHitsOptions,
  WatchRowsOptions,
  WhereInput,
} from '../query.ts';
export type { BatchSlot, BatchWriter, EntityTarget, LinkOptions } from '../batch.ts';
export type {
  AfterClock,
  EarsClock,
  WatchController,
  WatchHandle,
  WatchMode,
  WatchOptions,
} from '../watch.ts';
export type {
  BatchResult,
  CheckReport,
  DatabaseStats,
  DumpReport,
  ErrorCode,
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
export type { Hit } from 'elmdb/protocol/Hit.js';
export type { QueryFilter } from 'elmdb/protocol/QueryFilter.js';
export type { QueryScore } from 'elmdb/protocol/QueryScore.js';
export type { QueryStep } from 'elmdb/protocol/QueryStep.js';
export type { QueryTerminal } from 'elmdb/protocol/QueryTerminal.js';
export type { FusionMethod, SortDirection } from '../database.ts';

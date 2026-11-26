/**
 * `@elmdb/ears` — TypeScript v2 facade over elmdb: immutable algebraic query builder,
 * single-crossing `elmdb-wire` binary batch writer, clock-driven sans-I/O live watches,
 * and compile-time typed facades (`defineEars`).
 *
 * This root entrypoint never imports the native `elmdb` addon; open databases via `@elmdb/ears/database`.
 *
 * @packageDocumentation
 */

export {
  EarsError,
  isEarsError,
  toEarsError,
  RESERVED_ENTITY_TYPES,
  RESERVED_FIELDS,
  type EarsErrorCode,
} from './errors.ts';

export type {
  BuildPolicy,
  CommitDelta,
  DurabilityMode,
  EarsDatabase,
  EdgeDeltaEvent,
  EdgeDirection,
  EntityDeltaEvent,
  FusionMethod,
  SortDirection,
} from './database.ts';

export {
  BatchSlot,
  createBatchWriter,
  encodeBatch,
  type BatchWriter,
  type EntityTarget,
  type LinkOptions,
} from './batch.ts';

export {
  createSystemClock,
  type EarsClock,
  type WatchController,
  type WatchHandle,
  type WatchMode,
  type WatchOptions,
} from './watch.ts';

export type {
  DegreesOptions,
  EntityRow,
  FieldPredicate,
  FuseOptions,
  GraphPprResult,
  GraphTopoResult,
  HasLinkOptions,
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
  WhereInput,
} from './query.ts';

export {
  createEarsEngine,
  type BeforeCommitHook,
  type CreateEarsEngineOptions,
  type EarsAdmin,
  type EarsEngine,
  type GetEdgesOptions,
} from './engine.ts';

export {
  asEntityId,
  defineEars,
  type EARS,
  type EarsSchemaShape,
  type EntityTypeName,
  type RelationKindName,
  type TypedBatchWriter,
  type TypedBeforeCommitHook,
  type TypedEars,
  type TypedEntityRow,
  type TypedQueryRoot,
} from './typed.ts';

export type {
  BatchResult,
  CheckReport,
  DatabaseStats,
  DumpReport,
  ErrorCode,
  IndexStatus,
  NextDueWatch,
  OpenOptions,
  OpenReport,
  QueryAnswer,
  QuerySpec,
  RelationRow,
  Row,
  Schema,
  SchemaChange,
  WatchPolicyOptions,
  WatchRegistration,
  WatchRerun,
  WatchTick,
} from 'elmdb';

export type { FieldDefinition } from 'elmdb/protocol/FieldDefinition.js';
export type { FieldSpec } from 'elmdb/protocol/FieldSpec.js';
export type { Hit } from 'elmdb/protocol/Hit.js';
export type { IndexDefinition } from 'elmdb/protocol/IndexDefinition.js';
export type { ProtocolCardinality } from 'elmdb/protocol/ProtocolCardinality.js';
export type { ProtocolOnDelete } from 'elmdb/protocol/ProtocolOnDelete.js';
export type { QueryFilter } from 'elmdb/protocol/QueryFilter.js';
export type { QueryScore } from 'elmdb/protocol/QueryScore.js';
export type { QuerySource } from 'elmdb/protocol/QuerySource.js';
export type { QueryStep } from 'elmdb/protocol/QueryStep.js';
export type { QueryTerminal } from 'elmdb/protocol/QueryTerminal.js';
export type { RelationDefinition } from 'elmdb/protocol/RelationDefinition.js';
export type { RelationEndpoint } from 'elmdb/protocol/RelationEndpoint.js';
export type { TypeDefinition } from 'elmdb/protocol/TypeDefinition.js';

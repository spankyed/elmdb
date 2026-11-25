// The EARS engine: an instance you create over an elmdb database and hold, with its operations as its members
// (`engine.create(...)`) and the creator's privileges beside them (`engine.admin`). Nothing here acts on an engine you
// didn't pass: a program that binds one engine for code that holds none does that itself, over `defineEars`'s getter.
// The database is opened by `@elmdb/ears/database`, the one entry that loads elmdb.
export {
  createEarsEngine, type EarsEngine, type Ears, type EarsAdmin,
  type EntityRow, type DatabaseCatalog, type SchemaStats, type RelationMatch, type RelationRow, type RelationStats,
} from './engine.ts';
export type { BeforeCommit, ChangedEntities, ChangedPosition, EarsDatabase, PendingChanges, EarsUnit, OpenReport, SchemaChange, UnitOptions } from './database.ts';
export { WATCH_SCHEDULE } from './watch.ts';
export type {
  WatchCauseSource, WatchDiagnostics, WatchEntryDiagnostics, WatchObserver, WatchOptions, WatchReadContext,
  WatchSchedulerDiagnostics, Watching, WatchSchedule,
} from './watch.ts';
export type { CallCounting, CallKind, CallReport, CallSite, CallSiteNaming, WatchCount } from './calls.ts';
export type { EngineSchema, RankDeclaration, RelationRules } from './schema.ts';
export type { Create, Upsert, Write, TypeName } from './write.ts';
export { EarsError, RESERVED_ATTRIBUTES, RESERVED_ENTITY_TYPES, type EarsErrorCode } from './errors.ts';
export type {
  EARSRuntimeDeps, EarsClock, EarsWarning, EarsSchema, UntypedSchema, QueryBuilder, WriteBuilder, SearchDeclaration, SearchOptions, SearchHit, RankedOptions, RankBound, SearchHighlights, QueryExplanation, QueryStage, EdgeDirection, SimilarityMeasure, FieldRule, FieldType, EmbedDeclaration, PendingEmbeddings, Embedding, RelationEdge, LinkOptions, WeightedLinkOptions, RelationPatch, RankSeed, RankOptions, RowIdentity, ShortCodeIdentity, WriteOptions, WriteTarget, FieldValue, FieldValues, ListElement, WritableField, OptionalField, RequiredField, CreateFields, WhereValue, RowRules, ShortCode, NoInferType,
} from './runtime.ts';
export { b64Encode, b64Decode, type Query, type QueryEntries } from './query.ts';
export { entityTypeOf, getTimestamp } from './ids.ts';
export { EARS } from './entities.ts';
export type { EntityShapes, ShapeOf, EntityNameArg, BaseEntity } from './entities.ts';
export {
  defineEars,
  type RowArg, type TypedEars, type TypedQuery, type TypedQueryEntries, type TypedCreate, type TypedUpsert, type TypedWrite, type TypedNewId, type TypedGetAttr,
} from './typed.ts';

export type { EarsOperation, EarsOperationResult, EarsOperationObserver } from './observation.ts';

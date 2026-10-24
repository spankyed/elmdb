// The EARS engine: an instance you create and hold, with its operations as its members (`engine.create(...)`) and
// the creator's privileges beside them (`engine.admin`). Nothing here acts on an engine you didn't pass: the app's
// engine, and the operations over it that pack code calls, are the SDK's (`@apackdev/sdk/ears`).
export {
  createEarsEngine, type EarsEngine, type Ears, type EarsAdmin,
} from './engine.ts';
export type { Create, Upsert, Write, TypeName } from './write.ts';
export { EarsError, RESERVED_ATTRIBUTES, RESERVED_ENTITY_TYPES, type EarsErrorCode } from './errors.ts';
export type {
  EARSRuntimeDeps, EarsSchema, UntypedSchema, QueryBuilder, WriteBuilder, RelationEdge, LinkOptions, RowIdentity, ShortCodeIdentity, WriteOptions, WriteTarget, FieldValue, FieldValues, ListElement, WritableField, OptionalField, RequiredField, CreateFields, WhereValue, RowRules, ShortCode,
} from './runtime.ts';
export { b64Encode, b64Decode, type Query } from './query.ts';
export { entityTypeOf, type EntityRow, type SchemaStats } from './attribute-storage.ts';
export { getTimestamp } from './numbering.ts';
export type { RelationMatch, RelationRow, RelationStats } from './relations.ts';
// Types that appear in the signatures above, so pack authors can name them.
export type { PersistenceSink, PersistenceErrorStats } from './runtime.ts';
export { noopSink } from './runtime.ts';
// Record stores: whole records by id, found through declared indexes (the run history's store); one on disk is
// `@apackdev/ears/lmdb`'s `openRecordStore`
export {
  createMemoryRecordStore, _indexEntries, _idClock,
  type RecordStore, type RecordIndexes, type StoredRecord, type IndexValue, type FindByOptions,
} from './records.ts';
// The ordered key-value seam the engine is written on (`createEarsEngine({ kv })`): in memory for tests and tooling,
// and on disk through `@apackdev/ears/lmdb`
export {
  createMemoryKV, createMemoryKVBackend, createOrderedKV,
  type OrderedKV, type KvReader, type KvWriter, type Key, type KeyPart, type RangeOptions, type Visit,
  type KvBackend, type ByteReader, type ByteWriter,
} from './kv.ts';
export { EARS } from './entities.ts';
export type { EntityShapes, ShapeOf, EntityNameArg, BaseEntity } from './entities.ts';
export {
  defineEars,
  type TypedEars, type TypedQuery, type TypedCreate, type TypedUpsert, type TypedWrite, type TypedNewId, type TypedGetAttr,
} from './typed.ts';

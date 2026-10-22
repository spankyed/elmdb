// The EARS engine: an instance you create and hold, with its operations as its members (`engine.create(...)`) and
// the creator's privileges beside them (`engine.admin`). Nothing here acts on an engine you didn't pass: the app's
// engine, and the operations over it that pack code calls, are the SDK's (`@apackdev/sdk/ears`).
export {
  createEarsEngine, type EarsEngine, type Ears, type EarsAdmin,
} from './engine.ts';
export type { Create, Upsert, Write, TypeName } from './write.ts';
export { EarsError, RESERVED_ATTRIBUTES, type EarsErrorCode } from './errors.ts';
export type { EdgeStore, EdgeMatch } from './edge-store.ts';
export type { RelationIndex, RelationIndexEntry } from './relation-index.ts';
export type {
  EARSRuntimeDeps, QueryBuilder, WriteBuilder, LinkOptions, CreateOptions, FieldValue, FieldValues,
} from './runtime.ts';
export { b64Encode, b64Decode, type Query } from './query.ts';
export { entityTypeOf } from './attribute-storage.ts';
export { prepareEntity } from './transaction-helpers.ts';
export { RepositoryError, RepositoryErrorCode } from './repository-errors.ts';
export { getTimestamp, filterSystemFields } from './entity-utils.ts';
export type { RelationMatch, RelationRow, RelationStats } from './relations.ts';
// Types that appear in the signatures above, so pack authors can name them.
export type { CreatedEntityFields } from './transaction-helpers.ts';
export type { PersistenceSink, PersistenceErrorStats } from './runtime.ts';
export { noopSink } from './runtime.ts';
// The persistence port: partitions, their policy, and the sink routing writes to each partition's sink
export { makePolicy, type Partition, type PartitionPolicy } from './persistence/policy.ts';
export { makeShardedPersistence, type ShardedPersistence, type RelationDetailsReader } from './persistence/sharded-router.ts';
export { EARS } from './entities.ts';
export type { EntityShapes, ShapeOf, EntityNameArg, BaseEntity } from './entities.ts';
export {
  defineEars,
  type TypedEars, type TypedQuery, type TypedCreate, type TypedUpsert, type TypedWrite, type TypedFindById, type TypedFindAll, type TypedFindWhere, type TypedFindFirst, type TypedCreateEntity,
  type TypedGetAttr, type TypedGetAttrs, type TypedFindWithFields, type TypedFindByIdWithFields, type TypedFindWithRole,
  type TypedFindFirstWithRole, type TypedCreateEntityWithDefaults, type TypedUpdateEntity,
} from './typed.ts';
export { bp, type Blueprint, type BlueprintBuilder } from './blueprint.ts';

export {
  createEarsEngine, type EarsEngine, type EarsQuery, type EarsAdmin,
} from './engine.ts';
export { installEngine, installedEngine } from './installed.ts';
export type { EdgeStore, EdgeMatch } from './edge-store.ts';
export type { RelationIndex, RelationIndexEntry } from './relation-index.ts';
export type {
  EARSRuntimeDeps, QueryBuilder, TransactionBuilder, SafeLinkOptions, FieldValue, FieldValues,
} from './runtime.ts';
export { isEntityType } from './runtime.ts';
export { b64Encode, b64Decode, type QxStart } from './query.ts';
export {
  getEntitiesOfType, getAllEntityTypes, destroyEntity, removeRelationById,
  getAll, getAllAttributeKinds, getAllRelationKinds, getAttributeStats, getSchemaStats,
  getRoles, grantRole, revokeRole,
} from './attribute-storage.ts';
export { repository, registerRepository, unregisterRepository, type Repository } from './repository.ts';
export { hasIdCollision, exists, countEntities } from './query-helpers.ts';
export { createRelation, removeRelation, prepareEntity } from './transaction-helpers.ts';
export { RepositoryError, RepositoryErrorCode } from './repository-errors.ts';
export { getTimestamp, generateShortCode, generateLabelWithCount, filterSystemFields } from './entity-utils.ts';
export { findRelations, getRelationStats, type RelationMatch, type RelationRow, type RelationStats } from './relations.ts';
export { getAllEntities, queryEntitiesByAttribute, queryEntitiesByRelationTo, queryEntitiesInRelationTo } from './attribute-storage.ts';
/**
 * The untyped query: entity names and fields aren't checked. Pack code queries with the typed `qx`
 * from #generated/ears; this is the escape hatch for fields only known at runtime or that vary across
 * a union shape's members (TYPED-EARS.md).
 */
export { qx as untypedQx } from './query.ts';
/**
 * The untyped write, the twin of `untypedQx`: entity names and field values aren't checked. Pack code writes
 * with the typed `tx` from #generated/ears; this is for an entity named at runtime — a migration walking every
 * type, an applier, tooling — where there is no name for the compiler to check against.
 */
export { tx as untypedTx } from './transaction.ts';
// Types that appear in the signatures above, so pack authors can name them.
export type { CreatedEntityFields } from './transaction-helpers.ts';
export type { PersistenceSink, PersistenceErrorStats } from './runtime.ts';
// The persistence port: partitions, their policy, and the sink routing writes to each partition's sink
export { makePolicy, type Partition, type PartitionPolicy } from './persistence/policy.ts';
export { makeShardedPersistence, type ShardedPersistence, type RelationDetailsReader } from './persistence/sharded-router.ts';
export { EARS } from './entities.ts';
export type { EntityShapes, ShapeOf, EntityNameArg, BaseEntity } from './entities.ts';
export {
  defineEars,
  type TypedEars, type TypedQx, type TypedTx, type TypedFindById, type TypedFindAll, type TypedFindWhere, type TypedFindFirst, type TypedCreateEntity,
  type TypedGetAttr, type TypedGetAttrs, type TypedFindWithFields, type TypedFindByIdWithFields, type TypedFindWithRole,
  type TypedFindFirstWithRole, type TypedCreateEntityWithDefaults, type TypedUpdateEntity,
} from './typed.ts';
export { bp, spawn, type Blueprint, type BlueprintBuilder } from './blueprint.ts';
export {
  descendants, ancestors, rootParent, linkSymmetric, wouldCreateCycle,
  topoSort, shortestPath, leaves, lowestCommonAncestor,
} from './graph.ts';

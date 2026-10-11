// An EARS engine: its stores, indexes and caches, and the two faces over them. Holding the engine is the
// capability: `query` is what packs reach (through the installed engine), `admin` stays with whoever created it.
import type { EARS } from './entities.ts';
import { noopSink, type EARSRuntimeDeps } from './runtime.ts';
import { createRelationIndex, type RelationIndex } from './relation-index.ts';
import { createAttributeStorage } from './attribute-storage.ts';
import { createEdgeStore, type EdgeStore } from './edge-store.ts';
import { createQx, type qx } from './query.ts';
import { createGraph } from './graph.ts';
import { createTx, type tx } from './transaction.ts';
import { createQueryHelpers } from './query-helpers.ts';
import { createEntityCounters } from './entity-utils.ts';
import { createTransactionHelpers, type CreatedEntityFields } from './transaction-helpers.ts';
import { createRelationReads, type RelationMatch, type RelationRow, type RelationStats } from './relations.ts';
import { createSpawn, type Blueprint } from './blueprint.ts';
import { createRepositoryRegistry } from './repository.ts';

/** An engine's queries, transactions and repository registry: what packs use, through the installed engine */
export interface EarsQuery {
  /** Queries the engine, untyped (a pack's `#generated/ears` types it) */
  qx: typeof qx;
  /** Writes to the engine: creates an entity of a type, or changes the entity with an id */
  tx: typeof tx;
  /** Whether `name` is an entity type the engine's creator registered */
  isEntityType(name: string): boolean;
  findById<T = unknown>(id: EARS.EntityId): T | undefined;
  findByIdRaw<T = unknown>(id: EARS.EntityId): T | undefined;
  findAll<T = unknown>(entityType: EARS.Entity): T[];
  findWhere<T = unknown>(entityType: EARS.Entity, field: string, value: unknown): T[];
  findFirst<T = unknown>(entityType: EARS.Entity, field: string, value: unknown): T | undefined;
  findWithFields<T>(entityType: EARS.Entity, fields: string[]): T[];
  findByIdWithFields<T>(id: EARS.EntityId, fields: string[]): T | undefined;
  findWithRole<T>(entityType: EARS.Entity, role: string): T[];
  findFirstWithRole<T>(entityType: EARS.Entity, role: string): T | undefined;
  countEntities(entityType: EARS.Entity): number;
  exists(id: EARS.EntityId): boolean;
  hasIdCollision(providedId: string | undefined): boolean;
  createEntityWithDefaults<T extends Record<string, unknown> = Record<string, unknown>>(
    entityType: EARS.Entity, data: Partial<T>, prefix?: string, providedId?: EARS.EntityId,
  ): T & CreatedEntityFields;
  updateEntity(id: EARS.EntityId, updates: Record<string, unknown>, skipTimestamp?: boolean): void;
  createRelation(sourceId: EARS.EntityId, relationType: EARS.RelKind, targetId: EARS.EntityId): void;
  removeRelation(sourceId: EARS.EntityId, relationType: EARS.RelKind, targetId?: EARS.EntityId): void;
  getAttr(id: EARS.EntityId, kind: EARS.AttrKind, index?: number): EARS.AttributeValue | null;
  getAttrs(id: EARS.EntityId, kind: EARS.AttrKind): EARS.AttributeValue[];
  getAll(id: EARS.EntityId): Record<string, unknown>;
  getRoles(id: EARS.EntityId): string[];
  grantRole(id: EARS.EntityId, role: string): void;
  revokeRole(id: EARS.EntityId, role: string): void;
  getAllEntities(): EARS.EntityId[];
  getEntitiesOfType(type: EARS.Entity): EARS.EntityId[];
  getAllEntityTypes(): EARS.Entity[];
  getAllAttributeKinds(): EARS.AttrKind[];
  getAllRelationKinds(): string[];
  getAttributeStats(kind: EARS.AttrKind): { entityCount: number; totalValues: number };
  getSchemaStats(): { entities: Record<string, number>; attributes: Record<string, number>; relations: Record<string, number> };
  queryEntitiesByAttribute(kind: EARS.AttrKind, value?: unknown): EARS.EntityId[];
  queryEntitiesByRelationTo(relKind: string, id: EARS.EntityId, asSource?: boolean): EARS.EntityId[];
  queryEntitiesInRelationTo(target: EARS.EntityId): EARS.EntityId[];
  destroyEntity(id: EARS.EntityId, skipPersistence?: boolean): void;
  removeRelationById(relId: EARS.EntityId): void;
  findRelations(match?: RelationMatch): RelationRow[];
  getRelationStats(kind: EARS.RelKind): RelationStats;
  descendants(start: EARS.EntityId, relKind: EARS.RelKind): EARS.EntityId[];
  ancestors(start: EARS.EntityId, relKind: EARS.RelKind): EARS.EntityId[];
  rootParent(start: EARS.EntityId, relKind: EARS.RelKind): EARS.EntityId;
  wouldCreateCycle(src: EARS.EntityId, tgt: EARS.EntityId, kinds: readonly EARS.RelKind[]): boolean;
  linkSymmetric(a: EARS.EntityId, b: EARS.EntityId, kind: EARS.RelKind, info?: unknown): void;
  topoSort(roots: EARS.EntityId[], kind: EARS.RelKind, filterType?: EARS.Entity): EARS.EntityId[];
  shortestPath(src: EARS.EntityId, tgt: EARS.EntityId, kinds: EARS.RelKind[]): EARS.EntityId[] | null;
  leaves(kind: EARS.RelKind, filterType?: EARS.Entity): EARS.EntityId[];
  lowestCommonAncestor(a: EARS.EntityId, b: EARS.EntityId, treeKind: EARS.RelKind): EARS.EntityId | null;
  spawn(root: Blueprint, options?: { dedupe?: boolean }): EARS.EntityId;
  generateShortCode(entityType: EARS.Entity, prefix: string): string;
  generateLabelWithCount(baseLabel: string, entityType: EARS.Entity): string;
  /** The registered repositories; reading an unregistered name throws */
  repository: Record<string, unknown>;
  registerRepository(name: string, value: unknown): void;
  unregisterRepository(name: string): void;
}

/** An engine's data lifecycle and direct writes: for the code that created it (the app's composition root, tests, tooling) */
export interface EarsAdmin {
  /** Empties the engine's attributes, entity index and relation index */
  clear(): void;
  /** An attribute's value at an index (the first by default), or null */
  getAttr(id: EARS.EntityId, kind: EARS.AttrKind, index?: number): EARS.AttributeValue | null;
  /** Loads a stored attribute value (merging objects at an index), without telling persistence */
  bulkLoadAttr(id: EARS.EntityId, kind: EARS.AttrKind, value: unknown, index?: number): void;
  putAttr(id: EARS.EntityId, kind: EARS.AttrKind, value: unknown): void;
  addAttr(id: EARS.EntityId, kind: EARS.AttrKind, value: unknown): void;
  mergeAttr(id: EARS.EntityId, kind: EARS.AttrKind, value: unknown, index?: number): void;
  dropAttr(id: EARS.EntityId, kind: EARS.AttrKind, index?: number): void;
  dropIf(id: EARS.EntityId, kind: EARS.AttrKind, criterion: unknown): void;
  updateAttr(id: EARS.EntityId, kind: EARS.AttrKind, value: unknown): void;
  addRelation(src: EARS.EntityId, kind: string, tgt: EARS.EntityId, info?: unknown): EARS.EntityId;
  updateRelation(relId: EARS.EntityId, newSource?: EARS.EntityId, newTarget?: EARS.EntityId, info?: unknown): void;
  queryEntitiesByRole(role: string): EARS.EntityId[];
  edgeStore: EdgeStore;
  /** The relation index, by kind (read it; write through the index functions) */
  readonly relationIndex: Readonly<RelationIndex>;
  /** Indexes a stored relation, without telling persistence */
  addToIndex(kind: string, source: EARS.EntityId, target: EARS.EntityId, relId: EARS.EntityId): void;
  removeFromIndex(kind: string, source: EARS.EntityId, target: EARS.EntityId, relId: EARS.EntityId): void;
  updateIndex(kind: string, relId: EARS.EntityId, oldSource: EARS.EntityId, oldTarget: EARS.EntityId, newSource?: EARS.EntityId, newTarget?: EARS.EntityId): void;
  /** The engine's entity-type checker */
  isEntityType(name: string): boolean;
  /** A copy of the registered repositories, by name */
  repositories(): Record<string, unknown>;
}

/** An EARS engine */
export interface EarsEngine {
  query: EarsQuery;
  admin: EarsAdmin;
}

/**
 * A new, empty engine. `isEntityType` tells an entity type from an id (`tx('Note')` creates a Note);
 * `persistence` receives every write (nothing by default). The engine is used through its faces, or
 * through the free functions once its query face is installed (`installEngine`).
 */
export function createEarsEngine({ persistence = noopSink, isEntityType }: EARSRuntimeDeps): EarsEngine {
  const relations = createRelationIndex();
  const storage = createAttributeStorage({ relations, persistence });
  const edgeStore = createEdgeStore({ relations, storage });
  const qx = createQx({ storage, relations, isEntityType });
  const graph = createGraph({ qx, edgeStore });
  const tx = createTx({ storage, edgeStore, qx, graph, isEntityType });
  const finders = createQueryHelpers(qx);
  const counters = createEntityCounters(qx);
  const writers = createTransactionHelpers({ tx, counters });
  const relationReads = createRelationReads({ edgeStore, relations, storage });
  const repositories = createRepositoryRegistry();

  const query: EarsQuery = {
    qx, tx, isEntityType,
    ...finders,
    ...writers,
    ...counters,
    ...relationReads,
    ...graph,
    spawn: createSpawn(tx),
    getAttr: storage.getAttr,
    getAttrs: storage.getAttrs,
    getAll: storage.getAll,
    getRoles: storage.getRoles,
    grantRole: storage.grantRole,
    revokeRole: storage.revokeRole,
    getAllEntities: storage.getAllEntities,
    getEntitiesOfType: storage.getEntitiesOfType,
    getAllEntityTypes: storage.getAllEntityTypes,
    getAllAttributeKinds: storage.getAllAttributeKinds,
    getAllRelationKinds: storage.getAllRelationKinds,
    getAttributeStats: storage.getAttributeStats,
    getSchemaStats: storage.getSchemaStats,
    queryEntitiesByAttribute: storage.queryEntitiesByAttribute,
    queryEntitiesByRelationTo: storage.queryEntitiesByRelationTo,
    queryEntitiesInRelationTo: storage.queryEntitiesInRelationTo,
    destroyEntity: storage.destroyEntity,
    removeRelationById: storage.removeRelationById,
    repository: repositories.repository,
    registerRepository: repositories.registerRepository,
    unregisterRepository: repositories.unregisterRepository,
  };

  const admin: EarsAdmin = {
    clear: storage.clear,
    getAttr: storage.getAttr,
    bulkLoadAttr: storage.bulkLoadAttr,
    putAttr: storage.putAttr,
    addAttr: storage.addAttr,
    mergeAttr: storage.mergeAttr,
    dropAttr: storage.dropAttr,
    dropIf: storage.dropIf,
    updateAttr: storage.updateAttr,
    addRelation: storage.addRelation,
    updateRelation: storage.updateRelation,
    queryEntitiesByRole: storage.queryEntitiesByRole,
    edgeStore,
    relationIndex: relations.index,
    addToIndex: relations.addToIndex,
    removeFromIndex: relations.removeFromIndex,
    updateIndex: relations.updateIndex,
    isEntityType,
    repositories: repositories.entries,
  };

  return { query, admin };
}

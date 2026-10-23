// An EARS engine: its stores, indexes and caches, and the operations over them as the engine's own members, with the
// creator's privileges beside them in `admin`. A program is handed the engine typed as `Ears`, which has no `admin`:
// the faces define the contract, not a security boundary (packs run in-process).
import type { EARS } from './entities.ts';
import { noopSink, type EARSRuntimeDeps } from './runtime.ts';
import { createRelationIndex, type RelationIndex } from './relation-index.ts';
import { createAttributeStorage, type EntityRow, type SchemaStats } from './attribute-storage.ts';
import { createEdgeStore, type EdgeStore } from './edge-store.ts';
import { createQuery, type Query } from './query.ts';
import { createGraph } from './graph.ts';
import { createWrites, type Create, type Upsert, type Write } from './write.ts';
import { createQueryHelpers } from './query-helpers.ts';
import { createEntityCounters } from './entity-utils.ts';
import { createEntityHelpers, type CreatedEntityFields } from './entity-helpers.ts';
import { createRelationReads, type RelationMatch, type RelationRow, type RelationStats } from './relations.ts';
import { createSpawn, type Blueprint } from './blueprint.ts';
import { createRepositoryRegistry } from './repository.ts';
import { createJournal } from './journal.ts';
import { createNameChecks } from './names.ts';

/** An engine's operations — its queries, writes, transactions and repository registry: what a program is handed */
export interface Ears {
  /** Queries the engine, untyped (a pack's `#generated/ears` types it) */
  query: Query;
  /** Creates an entity of a registered type, with a new id or the one given */
  create: Create;
  /** Writes to the entity with an id, creating it as the type when no entity has it */
  upsert: Upsert;
  /** Writes to the entity with an id, which must exist */
  write: Write;
  /**
   * Runs `fn` as one unit: if it throws, every write it made is undone and none reaches persistence; if it
   * returns, persistence receives them together. Reads inside see its own writes. `fn` must be synchronous,
   * and a unit inside another commits into it.
   */
  transact<T>(fn: () => T): T;
  /** Whether `name` is an entity type the engine's creator registered */
  isEntityType(name: string): boolean;
  /** Whether `name` is a relation kind the engine's creator registered */
  isRelationKind(name: string): boolean;
  /** Whether `name` is a role the engine's creator registered */
  isRole(name: string): boolean;
  findById<T = unknown>(id: EARS.EntityId): T | undefined;
  findAll<T = unknown>(entityType: EARS.Entity): T[];
  findWhere<T = unknown>(entityType: EARS.Entity, field: string, value: unknown): T[];
  createEntityWithDefaults<T extends Record<string, unknown> = Record<string, unknown>>(
    entityType: EARS.Entity, data: Partial<T> & { id?: EARS.EntityId }, prefix?: string,
  ): T & CreatedEntityFields;
  /** An attribute's value, exactly as written, or null */
  getAttr(id: EARS.EntityId, kind: EARS.AttrKind): EARS.AttributeValue | null;
  /** The entity's row — its id, type and every attribute, each exactly as written, the trash included — or undefined */
  getRow(id: EARS.EntityId): EntityRow | undefined;
  getRoles(id: EARS.EntityId): string[];
  getAllEntityTypes(): EARS.Entity[];
  getAllAttributeKinds(): EARS.AttrKind[];
  getAllRelationKinds(): string[];
  /** Every role an entity holds, once each */
  getAllRoles(): string[];
  getAttributeStats(kind: EARS.AttrKind): { entityCount: number; totalValues: number };
  getSchemaStats(): SchemaStats;
  removeRelationById(relId: EARS.EntityId): void;
  /** Moves a relation to new ends, or replaces its info, keeping its id */
  updateRelation(relId: EARS.EntityId, patch: { sourceEntity?: EARS.EntityId; targetEntity?: EARS.EntityId; info?: unknown }): void;
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
  /** An attribute's value, or null */
  getAttr(id: EARS.EntityId, kind: EARS.AttrKind): EARS.AttributeValue | null;
  /** Every entity's id, the engine's own (relations, sequences) and those in the trash included: a raw listing */
  getAllEntities(): EARS.EntityId[];
  /** Every id of a type, the engine's own types and those in the trash included: a raw listing */
  getEntitiesOfType(type: EARS.Entity): EARS.EntityId[];
  /** Loads a stored attribute value, without telling persistence */
  bulkLoadAttr(id: EARS.EntityId, kind: EARS.AttrKind, value: unknown): void;
  /** Writes an attribute's value, any kind the engine owns included */
  putAttr(id: EARS.EntityId, kind: EARS.AttrKind, value: unknown): void;
  /** Removes an attribute */
  deleteAttr(id: EARS.EntityId, kind: EARS.AttrKind): void;
  addRelation(src: EARS.EntityId, kind: string, tgt: EARS.EntityId, info?: unknown): EARS.EntityId;
  updateRelation(relId: EARS.EntityId, newSource?: EARS.EntityId, newTarget?: EARS.EntityId, info?: unknown): void;
  edgeStore: EdgeStore;
  /** The relation index, by kind (read it; write through the index functions) */
  readonly relationIndex: Readonly<RelationIndex>;
  /** Indexes a stored relation, without telling persistence */
  addToIndex(kind: string, source: EARS.EntityId, target: EARS.EntityId, relId: EARS.EntityId): void;
  removeFromIndex(kind: string, source: EARS.EntityId, target: EARS.EntityId, relId: EARS.EntityId): void;
  updateIndex(kind: string, relId: EARS.EntityId, oldSource: EARS.EntityId, oldTarget: EARS.EntityId, newSource?: EARS.EntityId, newTarget?: EARS.EntityId): void;
  /** The engine's entity-type checker */
  isEntityType(name: string): boolean;
  /** The engine's relation-kind checker */
  isRelationKind(name: string): boolean;
  /** The engine's role checker */
  isRole(name: string): boolean;
  /** A copy of the registered repositories, by name */
  repositories(): Record<string, unknown>;
}

/** An EARS engine as its creator holds it: its operations, and the creator's privileges in `admin` */
export interface EarsEngine extends Ears {
  admin: EarsAdmin;
}

/**
 * A new, empty engine. `isEntityType` says which names are entity types `create` and `query` accept, and
 * `isRelationKind` which are relation kinds a link, and a read or walk over links, may name, and `isRole` which are roles
 * a role write or a query by role may name; `persistence` receives every write (nothing by default). Whoever holds the engine calls its operations on it;
 * the app's is the SDK's bound engine (`boundEars`, `@apackdev/sdk/ears`).
 */
export function createEarsEngine({ persistence = noopSink, isEntityType, isRelationKind, isRole }: EARSRuntimeDeps): EarsEngine {
  const names = createNameChecks({ isRelationKind, isRole });
  const { journal, unitSink, transact } = createJournal(persistence);
  const relations = createRelationIndex(journal);
  const storage = createAttributeStorage({ relations, persistence: unitSink, journal });
  const edgeStore = createEdgeStore({ relations, storage });
  const query = createQuery({ storage, isEntityType, names });
  const graph = createGraph({ query, edgeStore, names });
  const { create, upsert, write, updateRelation } = createWrites({ storage, edgeStore, graph, isEntityType, names, journal });
  const finders = createQueryHelpers(query);
  const counters = createEntityCounters(storage);
  const entityHelpers = createEntityHelpers({ create, counters });
  const relationReads = createRelationReads({ edgeStore, relations, storage, names });
  const repositories = createRepositoryRegistry();

  const operations: Ears = {
    query, create, upsert, write, transact, isEntityType, isRelationKind, isRole,
    ...finders,
    ...entityHelpers,
    ...counters,
    ...relationReads,
    ...graph,
    spawn: createSpawn({ create, query }),
    getAttr: storage.getAttr,
    getRow: storage.getRow,
    getRoles: storage.getRoles,
    getAllEntityTypes: storage.getAllEntityTypes,
    getAllAttributeKinds: storage.getAllAttributeKinds,
    getAllRelationKinds: storage.getAllRelationKinds,
    getAllRoles: storage.getAllRoles,
    getAttributeStats: storage.getAttributeStats,
    getSchemaStats: storage.getSchemaStats,
    removeRelationById: storage.removeRelationById,
    updateRelation,
    repository: repositories.repository,
    registerRepository: repositories.registerRepository,
    unregisterRepository: repositories.unregisterRepository,
  };

  const admin: EarsAdmin = {
    clear: storage.clear,
    getAttr: storage.getAttr,
    getAllEntities: storage.getAllEntities,
    getEntitiesOfType: storage.getEntitiesOfType,
    bulkLoadAttr: storage.bulkLoadAttr,
    putAttr: storage.putAttr,
    deleteAttr: storage.deleteAttr,
    addRelation: storage.addRelation,
    updateRelation: storage.updateRelation,
    edgeStore,
    relationIndex: relations.index,
    addToIndex: relations.addToIndex,
    removeFromIndex: relations.removeFromIndex,
    updateIndex: relations.updateIndex,
    isEntityType,
    isRelationKind,
    isRole,
    repositories: repositories.entries,
  };

  return { ...operations, admin };
}

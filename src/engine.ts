// An EARS engine: its stores, indexes and caches, and the operations over them as the engine's own members, with the
// creator's privileges beside them in `admin`. A program is handed the engine typed as `Ears`, which has no `admin`:
// the faces define the contract, not a security boundary (packs run in-process).
import { EARS } from './entities.ts';
import { noopSink, type EARSRuntimeDeps } from './runtime.ts';
import { createAttributeStorage, type EntityRow, type SchemaStats } from './attribute-storage.ts';
import { createQuery, type Query } from './query.ts';
import { createHierarchy } from './hierarchy.ts';
import { createWrites, type Create, type Upsert, type Write } from './write.ts';
import { createEntityCounters } from './numbering.ts';
import { createRelations, type RelationMatch, type RelationRow } from './relations.ts';
import { createJournal } from './journal.ts';
import { createMemoryKV } from './kv.ts';
import { createStoreAccess } from './store-access.ts';
import { createNameChecks } from './names.ts';

/** An engine's operations — its queries, writes and transactions: what a program is handed */
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
  /** The entity's row — its id, type and every attribute, each exactly as written, the trash included — or undefined */
  getRow(id: EARS.EntityId): EntityRow | undefined;
  /** One field of the entity's row, read as `getRow` reads it (the trash included), or undefined when it holds none */
  getAttr(id: EARS.EntityId, field: EARS.AttrKind): EARS.AttributeValue | undefined;
  getRoles(id: EARS.EntityId): string[];
  /** What the engine holds, keyed by the names its data holds: entities per type, attribute, relation and role use */
  getSchemaStats(): SchemaStats;
  /** The relation's row — its id, kind, ends and info — or undefined when no relation has the id */
  getRelation(relId: EARS.RelationId): RelationRow | undefined;
  /** The rows of the relations matching every end and kind named; with none named, every relation */
  findRelations(match?: RelationMatch): RelationRow[];
  /** Moves a relation to new ends, or replaces its info, keeping its id; refuses an id no relation has */
  updateRelation(relId: EARS.RelationId, patch: { source?: EARS.EntityId; target?: EARS.EntityId; info?: unknown }): void;
  /** Removes the relation with the id, refusing an id no relation has */
  removeRelation(relId: EARS.RelationId): void;
  /** Whether linking `source` to `target` would close a cycle through `kinds` */
  wouldCreateCycle(source: EARS.EntityId, target: EARS.EntityId, kinds: readonly EARS.RelKind[]): boolean;
  /** `<base> <n>`, numbered per type and base and never handed out twice: a default label that must be unique */
  nextLabel(type: EARS.Entity, base: string): string;
}

/** An engine's data lifecycle and direct writes: for the code that created it (the app's composition root, tests, tooling) */
export interface EarsAdmin {
  /** Empties the engine's attributes, entity index and relation index */
  clear(): void;
  /** Every entity's id, the engine's numbering sequences and those in the trash included: a raw listing */
  getAllEntities(): EARS.EntityId[];
  /** Every id of a type, the engine's own types and those in the trash included: a raw listing */
  getEntitiesOfType(type: EARS.Entity): EARS.EntityId[];
  /** Loads a stored attribute value, without telling persistence */
  bulkLoadAttr(id: EARS.EntityId, kind: EARS.AttrKind, value: unknown): void;
  /** Writes an attribute's value, any kind the engine owns included */
  putAttr(id: EARS.EntityId, kind: EARS.AttrKind, value: unknown): void;
  /** Removes an attribute */
  deleteAttr(id: EARS.EntityId, kind: EARS.AttrKind): void;
  /** Links any kind between any ids, either end missing included: unchecked, as loaded data is */
  addRelation(source: EARS.EntityId, kind: string, target: EARS.EntityId, info?: unknown): EARS.RelationId;
  /** `updateRelation` without its checks: any relation, to any ends */
  updateRelation(relId: EARS.RelationId, patch: { source?: EARS.EntityId; target?: EARS.EntityId; info?: unknown }): void;
  /** Loads a stored relation, without telling persistence */
  bulkLoadRelation(relId: EARS.RelationId, kind: string, source: EARS.EntityId, target: EARS.EntityId, info?: unknown): void;
  /** Runs `fn`, a load of stored rows, as one write unit */
  bulkLoad<T>(fn: () => T): T;
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
export function createEarsEngine({ kv = createMemoryKV(), persistence = noopSink, isEntityType, isRelationKind, isRole, rowRules = () => undefined, warn }: EARSRuntimeDeps): EarsEngine {
  const names = createNameChecks({ isRelationKind, isRole });
  const access = createStoreAccess(kv);
  const { journal, unitSink, transact } = createJournal(persistence, access);
  const storage = createAttributeStorage({ access, persistence: unitSink, inUnit: () => journal.open });
  const relations = createRelations({ access, persistence: unitSink });
  // The engine's own fields are indexed on every type; any other, where the type's row rules declare it
  const SHORT_CODE = EARS.AttrKind.Custom('shortCode');
  const indexed = (type: EARS.Entity, kind: EARS.AttrKind) =>
    kind === EARS.AttrKind.Role || kind === SHORT_CODE || (rowRules(type)?.indexes?.includes(kind) ?? false);
  const query = createQuery({ storage, relations, isEntityType, names, indexed, warn, snapshot: (fn) => access.read(fn) });
  const hierarchy = createHierarchy({ query, names });
  const counters = createEntityCounters(storage, access);
  const { create, upsert, write, updateRelation, removeRelation } = createWrites({ storage, relations, hierarchy, isEntityType, rowRules, counters, names, journal, access });

  const operations: Ears = {
    query, create, upsert, write, transact, isEntityType, isRelationKind, isRole,
    nextLabel: counters.nextLabel,
    ...hierarchy,
    getRow: storage.getRow,
    getAttr: (id, field) => (storage.hasAttr(id, field) ? storage.getAttr(id, field) : undefined),
    getRoles: storage.getRoles,
    getSchemaStats: () => ({ ...storage.entityStats(), relations: relations.stats() }),
    getRelation: relations.get,
    findRelations: (match = {}) => {
      if (match.kind !== undefined) names.relationKinds(match.kind, 'findRelations');
      return relations.find(match);
    },
    updateRelation,
    removeRelation,
  };

  const admin: EarsAdmin = {
    // A clear empties the store, relations and all; attribute storage refuses one inside a unit
    clear: storage.clear,
    getAllEntities: storage.getAllEntities,
    getEntitiesOfType: storage.getEntitiesOfType,
    bulkLoadAttr: storage.bulkLoadAttr,
    putAttr: storage.putAttr,
    deleteAttr: storage.deleteAttr,
    addRelation: relations.add,
    updateRelation: (relId, { source, target, info }) => relations.update(relId, source, target, info),
    bulkLoadRelation: relations.bulkLoad,
    bulkLoad: (fn) => access.write(fn),
  };

  return { ...operations, admin };
}

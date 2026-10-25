// An EARS engine: the operations over an elmdb database as the engine's own members, with the creator's privileges
// beside them in `admin`. A program is handed the engine typed as `Ears`, which has no `admin`: the faces define the
// contract, not a security boundary (packs run in-process). Everything the engine holds is in the database; the
// engine holds its connection, and the names its schema declares, read again when `admin.setSchema` changes them.
import type { EARS } from './entities.ts';
import type { EARSRuntimeDeps, Embedding, PendingEmbeddings, RelationEdge } from './runtime.ts';
import { createConnection, type Answer, type SchemaChange, type UnitOptions } from './database.ts';
import { createQuery, relationRow, type Query } from './query.ts';
import { createWrites, type Create, type Upsert, type Write } from './write.ts';
import { createNameChecks } from './names.ts';
import { declaredNames, toElmdbSchema, withDeclarations, type EngineSchema } from './schema.ts';
import { idSuffix } from './utils.ts';
import { RESERVED_ENTITY_TYPES } from './errors.ts';

/** An entity as a read gives it: its id and type, and every attribute it holds */
export type EntityRow = { id: EARS.EntityId; entityType: EARS.Entity } & Record<string, unknown>;

/** Which relations to match: any combination of kind, source and target */
export interface RelationMatch {
  kind?: EARS.RelKind;
  source?: EARS.EntityId;
  target?: EARS.EntityId;
}

/**
 * A relation row: its id (`removeRelation(id)` removes that one relation), kind, ends and info. The row
 * `RelationEdge` types, with its kind and ends untyped
 */
export type RelationRow = RelationEdge;

/** Counts for one relation kind */
export interface RelationStats {
  /** Relations of the kind */
  total: number;
  /** Distinct entities that are a source of one */
  uniqueSources: number;
  /** Distinct entities that are a target of one */
  uniqueTargets: number;
}

/** What an engine holds, each map keyed by a name its data holds */
export interface SchemaStats {
  /** Entities per type, the trash included */
  entities: Record<string, number>;
  /** Per attribute kind, the entities holding it and the values they hold (a list's elements each count) */
  attributes: Record<string, { entityCount: number; totalValues: number }>;
  /** Per relation kind, its relations and the distinct entities at each end */
  relations: Record<string, RelationStats>;
  /** Per role, the entities holding it */
  roles: Record<string, number>;
}

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
   * Runs `fn` as one unit: if it throws, every write it made is undone; if it returns, its writes commit together.
   * Reads inside see its own writes. `fn` must be synchronous, and a unit inside another commits into it. A
   * `durable` unit is on disk when it returns, rather than with the database's next sync.
   */
  transact<T>(fn: () => T, options?: UnitOptions): T;
  /** Whether `name` is an entity type the engine's schema declares */
  isEntityType(name: string): boolean;
  /** Whether `name` is a relation kind the engine's schema declares */
  isRelationKind(name: string): boolean;
  /** Whether `name` is a role the engine's schema declares */
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
  /** The entity types holding a vector index (a manifest entry's `embed`) */
  embeddedTypes(): string[];
  /** Up to `limit` (100 unless given) of the type's chunks waiting for embeddings, in key order, and how many wait in all */
  pendingEmbeddings(type: EARS.Entity, options?: { limit?: number }): PendingEmbeddings;
  /**
   * Stores embeddings of pending chunks, together: each one whose chunk's text has changed since `pendingEmbeddings`
   * answered is not stored, its chunk still pending under its new hash, and counted `stale`
   */
  embed(embeddings: readonly Embedding[]): { stored: number; stale: number };
}

/** An engine's data lifecycle and direct writes: for the code that created it (the app's composition root, tests, tooling) */
export interface EarsAdmin {
  /** Empties the engine's database, keeping its schema */
  clear(): void;
  /** Every entity's id, those in the trash included: a raw listing */
  getAllEntities(): EARS.EntityId[];
  /** Every id of a type, those in the trash included, whether or not the schema declares the type: a raw listing */
  getEntitiesOfType(type: EARS.Entity): EARS.EntityId[];
  /** How many entities each type holds, the trash included, without reading their attributes */
  countEntities(): Record<string, number>;
  /** Loads a stored attribute value as it was: unchecked, with no `updatedAt` stamp */
  bulkLoadAttr(id: EARS.EntityId, kind: EARS.AttrKind, value: unknown): void;
  /** Writes an attribute's value, any kind the engine owns included */
  putAttr(id: EARS.EntityId, kind: EARS.AttrKind, value: unknown): void;
  /** Removes an attribute, any kind the engine owns included */
  deleteAttr(id: EARS.EntityId, kind: EARS.AttrKind): void;
  /** Links any kind between any ids, either end missing included: unchecked, as loaded data is */
  addRelation(source: EARS.EntityId, kind: string, target: EARS.EntityId, info?: unknown): EARS.RelationId;
  /** `updateRelation` without its checks: any relation, to any ends */
  updateRelation(relId: EARS.RelationId, patch: { source?: EARS.EntityId; target?: EARS.EntityId; info?: unknown }): void;
  /** Loads a stored relation under its own id, unchecked */
  bulkLoadRelation(relId: EARS.RelationId, kind: string, source: EARS.EntityId, target: EARS.EntityId, info?: unknown): void;
  /** Runs `fn`, whose loads are applied together as one unit when it returns */
  bulkLoad<T>(fn: () => T): T;
  /**
   * Changes the database's schema to `schema` (elmdb's `setSchema`), answering each change and what was done about it:
   * an index added is built in the background, and a change elmdb refuses — a short code prefix changed — leaves
   * the schema as it was. Refused while a unit is open
   */
  setSchema(schema: EngineSchema): SchemaChange[];
  /**
   * Declares what `schema` declares — each of its types, relation kinds and roles, with their rules — keeping every other
   * declaration the database holds: what an app whose packs register one at a time applies as each does, so a pack's
   * registration changes nothing another pack declared. Answers the changes as `setSchema` does
   */
  declare(schema: EngineSchema): SchemaChange[];
}

/** An EARS engine as its creator holds it: its operations, and the creator's privileges in `admin` */
export interface EarsEngine extends Ears {
  admin: EarsAdmin;
}

/** A relation id for an unchecked load: a relation is no entity, but its id has an entity id's form */
const newRelationId = () => `${RESERVED_ENTITY_TYPES.relation}-${idSuffix()}` as EARS.RelationId;

/**
 * A new engine over `database`, which holds everything the engine does: its entity types, relation kinds and roles
 * are those the database's schema declares. Whoever holds the engine calls its operations on it; the app's is the
 * SDK's bound engine (`boundEars`, `@apackdev/sdk/ears`).
 */
export function createEarsEngine({ database, warn, freeze }: EARSRuntimeDeps): EarsEngine {
  const connection = createConnection(database, { freeze });
  let declared = declaredNames(database.schema);
  const isEntityType = (name: string) => declared.types.has(name);
  const isRelationKind = (name: string) => declared.relationKinds.has(name);
  const isRole = (name: string) => declared.roles.has(name);
  const names = createNameChecks({ isRelationKind, isRole });
  const read = connection.read;
  const one = (op: Record<string, unknown>): Answer => read([op])[0]!;

  const getRow = (id: EARS.EntityId) => (one({ read: 'row', id }).row as EntityRow | null) ?? undefined;
  const getAttr = (id: EARS.EntityId, field: EARS.AttrKind) => {
    const answer = one({ read: 'field', id, field });
    return answer.present ? answer.value as EARS.AttributeValue : undefined;
  };
  const getRelation = (relId: EARS.RelationId) => {
    const row = one({ read: 'relation', id: relId }).relation as Record<string, unknown> | null;
    return row ? relationRow(row) : undefined;
  };
  const findRelations = ({ kind, source, target }: RelationMatch = {}): RelationRow[] => {
    if (kind !== undefined) names.relationKinds(kind, 'findRelations');
    return (one({ read: 'findRelations', kind, source, target }).relations as Record<string, unknown>[]).map(relationRow);
  };

  const query = createQuery({
    read, isEntityType, names, warn,
    indexed: (type, field) => declared.indexed.get(type)?.has(field) ?? false,
  });
  const { create, upsert, write, updateRelation, removeRelation } = createWrites({
    isEntityType,
    write: connection.write,
    getRow,
    names,
    exists: (id) => {
      const [entity, relation] = read([{ read: 'field', id, field: 'entityType' }, { read: 'relation', id }]);
      return { entity: entity!.present === true, relation: relation!.relation !== null };
    },
  });

  const countEntities = (): Record<string, number> => Object.fromEntries(
    Object.entries(database.stats().types)
      .map(([type, { live, trashed }]) => [type, live + trashed] as const)
      .filter(([, n]) => n > 0),
  );

  const getSchemaStats = (): SchemaStats => {
    // Maps while counting, since a field may be named for an Object.prototype member (`__proto__`, `constructor`)
    const attributes = new Map<string, { entityCount: number; totalValues: number }>();
    const roles = new Map<string, number>();
    for (const row of query.withTrashed.all().pickAll() as EntityRow[]) {
      for (const [field, value] of Object.entries(row)) {
        if (field === 'id' || field === 'entityType') continue;
        let held = attributes.get(field);
        if (!held) attributes.set(field, held = { entityCount: 0, totalValues: 0 });
        held.entityCount++;
        held.totalValues += Array.isArray(value) ? value.length : 1;
        if (field === 'role') for (const role of new Set(value as string[])) roles.set(role, (roles.get(role) ?? 0) + 1);
      }
    }
    const kinds = new Map<string, { total: number; sources: Set<string>; targets: Set<string> }>();
    for (const { kind, source, target } of findRelations()) {
      let held = kinds.get(kind);
      if (!held) kinds.set(kind, held = { total: 0, sources: new Set(), targets: new Set() });
      held.total++;
      held.sources.add(source);
      held.targets.add(target);
    }
    return {
      entities: countEntities(),
      attributes: Object.fromEntries(attributes),
      relations: Object.fromEntries([...kinds].map(([kind, { total, sources, targets }]) => [kind, { total, uniqueSources: sources.size, uniqueTargets: targets.size }])),
      roles: Object.fromEntries(roles),
    };
  };

  const operations: Ears = {
    query, create, upsert, write, isEntityType, isRelationKind, isRole,
    transact: connection.transact,
    nextLabel: (type, base) => (connection.write([{ op: 'takeLabel', type, base }])[0] as { label: string }).label,
    wouldCreateCycle: (source, target, kinds) => {
      names.relationKinds(kinds, 'wouldCreateCycle');
      return one({ read: 'wouldCreateCycle', source, target, kinds: [...kinds] }).value as boolean;
    },
    embeddedTypes: () => [...declared.embedded],
    pendingEmbeddings: (type, { limit = 100 } = {}) => {
      const { model, dims, count, chunks } = one({ read: 'pendingEmbeddings', type, limit });
      return { model, dims, count, chunks } as PendingEmbeddings;
    },
    embed(embeddings) {
      if (!embeddings.length) return { stored: 0, stale: 0 };
      const results = connection.write(embeddings.map(({ id, chunk, hash, model, vector }) => ({ op: 'embed', id, chunk, hash, model, vector: [...vector] })));
      const stale = results.filter((result) => result.kind === 'stale').length;
      return { stored: results.length - stale, stale };
    },
    getRow,
    getAttr,
    getRoles: (id) => (getAttr(id, 'role' as EARS.AttrKind) as string[] | undefined) ?? [],
    getSchemaStats,
    getRelation,
    findRelations,
    updateRelation,
    removeRelation,
  };

  // Unchecked loads: sent together when `bulkLoad`'s function returns, and each on its own outside one
  let loading: Array<Record<string, unknown>> | undefined;
  const load = (item: Record<string, unknown>) => {
    if (loading) loading.push(item);
    else connection.load([item]);
  };
  const entityLoad = (id: EARS.EntityId, kind: EARS.AttrKind, value: unknown) =>
    load({ load: 'entity', id, fields: { [kind]: value }, trashed: false });

  const admin: EarsAdmin = {
    clear() {
      if (connection.inUnit) throw new Error('clear() inside transact(): emptying the engine is not a write a unit can undo');
      database.reset();
    },
    getAllEntities: () => query.withTrashed.all().ids(),
    // From every entity, so a type the schema no longer declares is listed too
    getEntitiesOfType: (type) => {
      const answer = one({ read: 'query', plan: { scope: 'withTrashed', start: { from: 'all' }, steps: [{ step: 'ofType', type }], terminal: { answer: 'ids' } } });
      return answer.ids as EARS.EntityId[];
    },
    countEntities,
    bulkLoadAttr: entityLoad,
    putAttr: entityLoad,
    deleteAttr: (id, kind) => load({ load: 'unset', id, field: kind }),
    addRelation(source, kind, target, info) {
      const id = newRelationId();
      load({ load: 'relation', id, kind, source, target, info });
      return id;
    },
    updateRelation(relId, { source, target, info }) {
      const held = getRelation(relId);
      if (!held) return;
      load({
        load: 'relation', id: relId, kind: held.kind,
        source: source || held.source, target: target || held.target,
        info: info !== undefined ? info : held.info,
      });
    },
    bulkLoadRelation: (relId, kind, source, target, info) => load({ load: 'relation', id: relId, kind, source, target, info }),
    bulkLoad<T>(fn: () => T): T {
      if (loading) return fn();
      loading = [];
      try {
        const value = fn();
        if (loading.length) connection.load(loading);
        return value;
      } finally {
        loading = undefined;
      }
    },
    setSchema(schema) {
      const changes = database.setSchema(toElmdbSchema(schema));
      declared = declaredNames(database.schema);
      return changes;
    },
    declare(schema) {
      const changes = database.setSchema(withDeclarations(database.schema, toElmdbSchema(schema)));
      declared = declaredNames(database.schema);
      return changes;
    },
  };

  return { ...operations, admin };
}


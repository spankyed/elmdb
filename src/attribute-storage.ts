import type { RelationIndexStore } from './relation-index.ts';
import { EARS } from './entities.ts';
import { idSuffix, isPlainObject } from './utils.ts';
import type { Journal } from './journal.ts';
import type { PersistenceSink } from './runtime.ts';
import { EarsError } from './errors.ts';

/**
 * Creates an id for a new entity of type `t`. A pack's `#generated/ears` exports this
 * typed to brand the id with the entity type, so reads through its helpers infer the shape.
 *
 * @internal
 */
export const newId = <E extends EARS.Entity>(t: E): EARS.EntityId =>
  `${t}-${idSuffix()}` as EARS.EntityId;

const ENTITY_TYPE = 'entityType' as EARS.AttrKind;

/** When an entity was put in the trash: an entity holding it is trashed, and reads by type, id or link skip it */
export const TRASHED_AT = 'trashedAt' as EARS.AttrKind;

/** No value: an attribute an entity doesn't hold, which `null`, a value, can't say */
const ABSENT = Symbol('absent');

/**
 * An entity's type, from its id: the one record of it. `newId` builds `<Type>-<suffix>`, `create`
 * refuses an id naming another type, and the id is the wire's record too — a frontend opens an entity by
 * it, and reference nodes keep ids inside stored documents — so nothing else stores the type. `entityIndex`
 * is keyed by this, and the `entityType` field every row carries is read from it.
 */
export const entityTypeOf = (id: EARS.EntityId) => {
  const dash = id.indexOf('-');
  return dash === -1 ? id as unknown as EARS.Entity : id.substring(0, dash) as EARS.Entity;
};

/** An engine's attributes (by kind, then entity), its entity index by type, and the writes and reads over them */
export function createAttributeStorage({ relations, persistence, journal }: { relations: RelationIndexStore; persistence: PersistenceSink; journal: Journal }) {
  // One value per attribute: a list is an array value. A stored value is never changed in place, so the value an
  // undo or a held sink call keeps is the value that was there
  const store       = new Map<EARS.AttrKind, Map<EARS.EntityId, EARS.AttributeValue>>();
  // Each type's entities, in the order they entered, with their place in that order: a read from the value index sorts
  // its matches by it, so they come in the order a walk of the type gives
  const entityIndex = new Map<EARS.Entity, Map<EARS.EntityId, number>>();
  let nextOrder = 0;
  // The entities holding `trashedAt`, kept by `place`, so what undo and hydration write keeps it too
  const trashed = new Set<EARS.EntityId>();
  /**
   * The value index, per attribute kind: a value → the entities holding it, or holding a list with it as an element
   * (multikey). Built for a kind on its first read (`holders`), then kept by `place`, the one write every change —
   * a unit's undo, hydration, a destroy — goes through. An object value isn't indexed.
   */
  // A value one entity holds keeps that id rather than a set of one: most values of a unique field (a title, a short
  // code) are held once, and a set per value is what a write would otherwise allocate
  const valueIndex = new Map<EARS.AttrKind, Map<unknown, EARS.EntityId | Set<EARS.EntityId>>>();
  // What `holders` answers for a value nobody holds; never written to, since only `indexValue` adds and it adds to its own
  const none: ReadonlySet<EARS.EntityId> = new Set();
  const relationIndex = relations.index;
  // A relation's details are its row in the relations table (`onAddRelation`, `onUpdateRelation`), which is
  // what hydration rebuilds the attribute from; storing the attribute too would be a second record of it
  const persisted = (kind: EARS.AttrKind) => kind !== EARS.AttrKind.RelationDetails;
  // Counts the writes that take entities out of the engine, so a query kept across them knows to check its ids
  let removals = 0;

  function clear() {
    if (journal.open) throw new Error('clear() inside transact(): emptying the engine is not a write a unit can undo');
    removals++;
    store.clear();
    entityIndex.clear();
    trashed.clear();
    valueIndex.clear();
    relations.clear();
  }

  const bucket = (k: EARS.AttrKind) => {
    if (!store.has(k)) store.set(k, new Map());
    return store.get(k)!;
  };

  const indexKey = (index: Map<unknown, EARS.EntityId | Set<EARS.EntityId>>, id: EARS.EntityId, key: unknown, add: boolean) => {
    // An object is never a key, whether the value or a list's element
    if (key !== null && typeof key === 'object') return;
    const held = index.get(key);
    if (add) {
      if (held === undefined) index.set(key, id);
      else if (typeof held === 'string') { if (held !== id) index.set(key, new Set([held, id])); }
      else held.add(id);
    } else if (held === id) {
      index.delete(key);
    } else if (typeof held !== 'string' && held?.delete(id) && held.size === 1) {
      index.set(key, held.values().next().value!);
    }
  };

  /** Indexes a value under itself, or a list under each of its elements */
  const indexValue = (index: Map<unknown, EARS.EntityId | Set<EARS.EntityId>>, id: EARS.EntityId, value: unknown, add: boolean) => {
    if (Array.isArray(value)) for (const element of value) indexKey(index, id, element, add);
    else indexKey(index, id, value, add);
  };

  /** The entities holding `value` under `kind`, or a list with it (a primitive: an object is never indexed) */
  const holders = (kind: EARS.AttrKind, value: unknown): ReadonlySet<EARS.EntityId> => {
    let index = valueIndex.get(kind);
    if (!index) {
      valueIndex.set(kind, index = new Map());
      for (const [id, v] of store.get(kind) ?? []) indexValue(index, id, v, true);
    }
    const held = index.get(value);
    return held === undefined ? none : typeof held === 'string' ? new Set([held]) : held;
  };

  /** Where the entity sits in its type's order */
  const orderOf = (id: EARS.EntityId) => entityIndex.get(entityTypeOf(id))?.get(id) ?? Infinity;

  /** Memory's one write of an attribute: `next`, or nothing when it is `ABSENT`. Undo goes through it too */
  const place = (id: EARS.EntityId, kind: EARS.AttrKind, next: EARS.AttributeValue | typeof ABSENT) => {
    let b = store.get(kind);
    if (valueIndex.size > 0) {
      const index = valueIndex.get(kind);
      if (index) {
        if (b?.has(id)) indexValue(index, id, b.get(id), false);
        if (next !== ABSENT) indexValue(index, id, next, true);
      }
    }
    if (next === ABSENT) b?.delete(id);
    else {
      if (!b) store.set(kind, b = new Map());
      b.set(id, next);
    }
    if (kind !== TRASHED_AT) return;
    if (next === ABSENT) trashed.delete(id);
    else if (!trashed.has(id)) {
      trashed.add(id);
      // A query kept across a trash drops the row at its next step, as it does a destroyed one
      removals++;
    }
  };

  /** Records how to put back the entity's value of a kind */
  const touchValue = (id: EARS.EntityId, kind: EARS.AttrKind) =>
    journal.open && journal.touch(`a\x1F${kind}\x1F${id}`, () => {
      const b = store.get(kind);
      const before = b?.has(id) ? b.get(id)! : ABSENT;
      return () => place(id, kind, before);
    });

  /** Records whether the entity was in the index, to put that back */
  const touchIndexed = (id: EARS.EntityId) =>
    journal.open && journal.touch(`e\x1F${id}`, () => {
      const type = entityTypeOf(id);
      const was = entityIndex.get(type)?.has(id) ?? false;
      return () => {
        if (was) enter(id);
        else if (leave(id)) removals++;
      };
    });

  /** Puts the entity in its type's index, last */
  const enter = (id: EARS.EntityId, type = entityTypeOf(id)) => {
    const ids = entityIndex.get(type);
    if (ids) {
      if (!ids.has(id)) ids.set(id, nextOrder++);
    } else {
      entityIndex.set(type, new Map([[id, nextOrder++]]));
    }
  };

  /** Takes the entity out of its type's index, and its type with it once it holds no other; whether it was there */
  const leave = (id: EARS.EntityId) => {
    const type = entityTypeOf(id);
    const ids = entityIndex.get(type);
    if (!ids?.delete(id)) return false;
    if (ids.size === 0) entityIndex.delete(type);
    return true;
  };

  const unindexEntity = (id: EARS.EntityId) => {
    if (!entityIndex.get(entityTypeOf(id))?.has(id)) return;
    touchIndexed(id);
    leave(id);
  };

  const indexEntity = (id: EARS.EntityId) => {
    const type = entityTypeOf(id);
    if (entityIndex.get(type)?.has(id)) return;
    touchIndexed(id);
    enter(id, type);
  };

  /** Every write of an attribute: journaled, in memory, and told to persistence */
  const assign = (id: EARS.EntityId, kind: EARS.AttrKind, next: EARS.AttributeValue | typeof ABSENT) => {
    if (next === ABSENT && !store.get(kind)?.has(id)) return;
    touchValue(id, kind);
    place(id, kind, next);
    if (next !== ABSENT) indexEntity(id);
    if (!persisted(kind)) return;
    if (next === ABSENT) persistence.onDropAttr(kind, id);
    else persistence.onPutAttr(kind, id, next);
  };

  const current = (id: EARS.EntityId, kind: EARS.AttrKind) => {
    const b = store.get(kind);
    return b?.has(id) ? b.get(id) : ABSENT;
  };

  /** The attribute's list, refusing a value that isn't one */
  const listOf = (id: EARS.EntityId, kind: EARS.AttrKind, via: string): readonly unknown[] => {
    const value = current(id, kind);
    if (value === ABSENT) return [];
    if (Array.isArray(value)) return value;
    throw new EarsError('NOT_A_LIST', `${via}('${kind}'): ${id}'s value is not a list`);
  };

  const put = (id: EARS.EntityId, kind: EARS.AttrKind, val: unknown) =>
    assign(id, kind, val as EARS.AttributeValue);

  /** Appends `val` to the attribute's list, starting one when there is none */
  const append = (id: EARS.EntityId, kind: EARS.AttrKind, val: unknown) =>
    assign(id, kind, [...listOf(id, kind, 'add'), val] as unknown as EARS.AttributeValue);

  /** Merges `val` into the attribute's object, or sets it when either isn't a plain object */
  const merge = (id: EARS.EntityId, kind: EARS.AttrKind, val: unknown) => {
    const cur = current(id, kind);
    const next = cur !== ABSENT && isPlainObject(cur) && isPlainObject(val) ? { ...cur, ...val } : val;
    assign(id, kind, next as EARS.AttributeValue);
  };

  /** Removes the list's element at `idx` */
  const removeAt = (id: EARS.EntityId, kind: EARS.AttrKind, idx: number) => {
    const list = listOf(id, kind, 'deleteAt');
    if (idx < 0 || idx >= list.length) return;
    assign(id, kind, list.filter((_, i) => i !== idx) as unknown as EARS.AttributeValue);
  };

  /** Removes the list's first element equal to `crit` (for an object, the first holding each of its fields) */
  const removeValue = (id: EARS.EntityId, kind: EARS.AttrKind, crit: unknown) => {
    const list = listOf(id, kind, 'deleteValue');
    const i = list.findIndex(
      v =>
        v === crit ||
        (isPlainObject(v) &&
          isPlainObject(crit) &&
          Object.entries(crit).every(([k, v0]) => (v as Record<string, unknown>)[k] === v0)),
    );
    if (i !== -1) assign(id, kind, list.filter((_, j) => j !== i) as unknown as EARS.AttributeValue);
  };

  /** Removes the attribute */
  const deleteAttr = (id: EARS.EntityId, kind: EARS.AttrKind) => assign(id, kind, ABSENT);

  /** A stored value, loaded without telling persistence */
  function bulkLoadAttr(id: EARS.EntityId, kind: EARS.AttrKind, val: unknown) {
    place(id, kind, val as EARS.AttributeValue);
    indexEntity(id);
  }

  // Roles are one list; an entity with none holds no role attribute
  const grantRole = (id: EARS.EntityId, role: string) => {
    const roles = getRoles(id);
    if (!roles.includes(role)) assign(id, EARS.AttrKind.Role, [...roles, role] as unknown as EARS.AttributeValue);
  };
  const revokeRole = (id: EARS.EntityId, role: string) => {
    const roles = getRoles(id);
    if (!roles.includes(role)) return;
    const rest = roles.filter((r) => r !== role);
    assign(id, EARS.AttrKind.Role, rest.length ? rest as unknown as EARS.AttributeValue : ABSENT);
  };

  function addRelation(
    src: EARS.EntityId,
    kind: string,
    tgt: EARS.EntityId,
    info?: unknown,
  ) {
    const entry = relationIndex[kind];
    if (entry?.bySource?.[src] && entry?.byTarget?.[tgt]) {
      const fromSource = new Set(entry.bySource[src]);
      const fromTarget = new Set(entry.byTarget[tgt]);
      for (const existingRelId of fromSource) {
        if (fromTarget.has(existingRelId)) {
          const existingRel = getAttr(existingRelId, EARS.AttrKind.RelationDetails) as EARS.RelationDetail;
          // The same edge: linking it again is a no-op, which keeps its id and its place among its siblings
          if (info === undefined || JSON.stringify(existingRel.info) === JSON.stringify(info)) return existingRelId;
        }
      }
    }
    const relId = newId(EARS.Entity.Relation);
    put(relId, EARS.AttrKind.RelationDetails, {
      sourceEntity: src,
      targetEntity: tgt,
      relationType: kind,
      info,
    } as EARS.RelationDetail);
    relations.addToIndex(kind, src, tgt, relId);
    persistence.onAddRelation(relId, kind, src, tgt, info);
    return relId;
  }

  function updateRelation(
    relId: EARS.EntityId,
    newS?: EARS.EntityId,
    newT?: EARS.EntityId,
    info?: unknown,
  ) {
    const d = getAttr(
      relId,
      EARS.AttrKind.RelationDetails,
    ) as EARS.RelationDetail | null;
    if (!d) return;
    const { sourceEntity: oS, targetEntity: oT, relationType: k } = d;
    const next: EARS.RelationDetail = {
      ...d,
      ...(newS && { sourceEntity: newS }),
      ...(newT && { targetEntity: newT }),
      ...(info !== undefined && { info }),
    };
    put(relId, EARS.AttrKind.RelationDetails, next);
    if (newS || newT)
      relations.updateIndex(k, relId, oS, oT, next.sourceEntity, next.targetEntity);
    const patch: any = {};
    if (newS) patch.src = newS;
    if (newT) patch.tgt = newT;
    if (info !== undefined) patch.info = info;
    persistence.onUpdateRelation(relId, patch);
  }

  /**
   * Removes a relation from memory and tells the sink. `destroyed` is the entity being deleted when that's why
   * the relation goes (destroyEntity); it's passed on so the sink can tell a deletion from an unlink.
   */
  const removeRelation = (relId: EARS.EntityId, destroyed?: EARS.EntityId) => {
    const d = getAttr(
      relId,
      EARS.AttrKind.RelationDetails,
    ) as EARS.RelationDetail | null;
    if (d)
      relations.removeFromIndex(d.relationType, d.sourceEntity, d.targetEntity, relId);
    deleteAttr(relId, EARS.AttrKind.RelationDetails);
    // A relation is its details: with them gone, so is the relation's entity
    unindexEntity(relId);
    if (destroyed) persistence.onRemoveRelation(relId, destroyed);
    else persistence.onRemoveRelation(relId);
  };
  const removeRelationById = (relId: EARS.EntityId) => removeRelation(relId);

  // `entityType` is read from the id rather than stored, so every row carries it and none can disagree
  const getAttr = (id: EARS.EntityId, k: EARS.AttrKind): EARS.AttributeValue | null => {
    if (k === ENTITY_TYPE) return hasEntity(id) ? entityTypeOf(id) : null;
    const value = current(id, k);
    return value === ABSENT ? null : value;
  };
  /** Whether the entity holds the attribute, `null` included */
  const hasAttr = (id: EARS.EntityId, k: EARS.AttrKind) =>
    k === ENTITY_TYPE ? hasEntity(id) : store.get(k)?.has(id) ?? false;
  const getRoles = (id: EARS.EntityId): string[] => {
    const roles = current(id, EARS.AttrKind.Role);
    return roles === ABSENT ? [] : roles as unknown as string[];
  };

  /** The entity's row — its id, type and every attribute, each exactly as written — or undefined when no entity has the id */
  const getRow = (id: EARS.EntityId): EntityRow | undefined => {
    if (!hasEntity(id)) return undefined;
    const row: EntityRow = { id, entityType: entityTypeOf(id) };
    for (const [k, b] of store) if (b.has(id)) row[k] = b.get(id);
    return row;
  };

  const getAllEntities = () => {
    const all: EARS.EntityId[] = [];
    for (const ids of entityIndex.values()) {
      for (const id of ids.keys()) all.push(id);
    }
    return all;
  };

  const getEntitiesOfType = (t: EARS.Entity) =>
    [...(entityIndex.get(t)?.keys() ?? [])];

  /** Whether the entity is in the engine, from its type's index rather than a scan of every entity */
  const hasEntity = (id: EARS.EntityId) =>
    entityIndex.get(entityTypeOf(id))?.has(id) ?? false;
  /** Whether the entity is in the trash */
  const isTrashed = (id: EARS.EntityId) => trashed.has(id);
  /** Whether the entity is in the engine and not in the trash: what a read by type, id or link reaches */
  const isLive = (id: EARS.EntityId) => hasEntity(id) && !trashed.has(id);

  const findEntitiesInRelationTo = (target: EARS.EntityId) => {
    const out = new Set<EARS.EntityId>();
    for (const k of Object.keys(relationIndex)) {
      const { bySource, byTarget } = relationIndex[k];
      bySource[target]?.forEach(relId => {
        const { targetEntity } = getAttr(
          relId,
          EARS.AttrKind.RelationDetails,
        ) as EARS.RelationDetail;
        out.add(targetEntity);
      });
      byTarget[target]?.forEach(relId => {
        const { sourceEntity } = getAttr(
          relId,
          EARS.AttrKind.RelationDetails,
        ) as EARS.RelationDetail;
        out.add(sourceEntity);
      });
    }
    return [...out];
  };

  const findEntitiesByRelationTo = (
    relKind: string,
    id: EARS.EntityId,
    asSource = false,
  ) => {
    const dir = relationIndex[relKind];
    if (!dir) return [];
    const relIds = asSource ? dir.bySource[id] ?? [] : dir.byTarget[id] ?? [];
    return relIds
      .map(rel => {
        const d = getAttr(
          rel,
          EARS.AttrKind.RelationDetails,
        ) as EARS.RelationDetail;
        return asSource ? d.targetEntity : d.sourceEntity;
      })
      .filter(Boolean);
  };

  function destroyEntity(id: EARS.EntityId, skipPersistence = false) {
    // A relation is its details, so destroying one is removing it, as an unlink of that one edge would
    if (entityTypeOf(id) === EARS.Entity.Relation) return removeRelation(id);
    removals++;
    for (const k of Object.keys(relationIndex)) {
      const { bySource, byTarget } = relationIndex[k];
      const relIds = [...(bySource[id] ?? []), ...(byTarget[id] ?? [])];
      // Every link to or from the entity goes from memory; the sink is told it's because `id` is being deleted
      for (const relId of relIds) removeRelation(relId, id);
      relations.forgetEntity(k, id);
    }
    for (const [k, b] of store) {
      if (!b.has(id)) continue;
      touchValue(id, k);
      place(id, k, ABSENT);
    }
    unindexEntity(id);
    if (!skipPersistence) {
      persistence.onDestroyEntity(id);
    }
  }

  const getAllAttributeKinds = (): EARS.AttrKind[] => Array.from(store.keys());
  const getAllRelationKinds = (): string[] => Object.keys(relationIndex);
  /** Every role an entity holds, once each */
  const getAllRoles = (): string[] => [...new Set([...(store.get(EARS.AttrKind.Role)?.values() ?? [])].flat() as string[])];
  const getAllEntityTypes = (): EARS.Entity[] => Array.from(entityIndex.keys());

  /** How many entities hold the attribute, and how many values they hold (a list's elements each count) */
  const getAttributeStats = (kind: EARS.AttrKind) => {
    const b = bucket(kind);
    let totalValues = 0;
    for (const value of b.values()) totalValues += Array.isArray(value) ? value.length : 1;
    return { entityCount: b.size, totalValues };
  };

  /** Tells the sink an entity was created */
  const entityCreated = (id: EARS.EntityId, entityType: EARS.Entity) => persistence.onCreateEntity(id, entityType);

  /** The engine's data counted: entities per type, each attribute kind's use and each relation kind's */
  function getSchemaStats(): SchemaStats {
    return {
      entities: Object.fromEntries(getAllEntityTypes().map((type) => [type, entityIndex.get(type)?.size ?? 0])),
      attributes: Object.fromEntries(getAllAttributeKinds().map((kind) => [kind, getAttributeStats(kind)])),
      relations: Object.fromEntries(Object.entries(relationIndex).map(([kind, { bySource, byTarget }]) => [kind, {
        totalRelations: Object.values(bySource).reduce((total, relIds) => total + relIds.length, 0),
        uniqueSources: Object.keys(bySource).length,
        uniqueTargets: Object.keys(byTarget).length,
      }])),
    };
  }

  return {
    clear, bulkLoadAttr,
    putAttr: put, appendAttr: append, mergeAttr: merge, removeAt, removeValue, deleteAttr,
    entityCreated, grantRole, revokeRole, addRelation, updateRelation, removeRelationById,
    getAttr, hasAttr, getRoles, getRow, getAllEntities, getEntitiesOfType, hasEntity, isTrashed, isLive, holders, orderOf, removals: () => removals,
    findEntitiesInRelationTo, findEntitiesByRelationTo,
    destroyEntity, getAllAttributeKinds, getAllRelationKinds, getAllRoles, getAllEntityTypes, getAttributeStats, getSchemaStats,
  };
}

export type AttributeStorage = ReturnType<typeof createAttributeStorage>;

/** An entity as a read gives it: its id and type, and every attribute it holds */
export type EntityRow = { id: EARS.EntityId; entityType: EARS.Entity } & Record<string, unknown>;

/** Entities per type, and each attribute kind's and relation kind's use, over a whole engine */
export interface SchemaStats {
  entities: Record<string, number>;
  attributes: Record<string, { entityCount: number; totalValues: number }>;
  relations: Record<string, { totalRelations: number; uniqueSources: number; uniqueTargets: number }>;
}

/** Whether a stored value is `v`, or a list containing it: what `where(k, v)` matches */
export const holds = (value: unknown, v: unknown) =>
  value === v || (Array.isArray(value) && value.includes(v));

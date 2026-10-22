import type { RelationIndexStore } from './relation-index.ts';
import { EARS } from './entities.ts';
import { idSuffix, isPlainObject } from './utils.ts';
import type { Journal } from './journal.ts';
import type { PersistenceSink } from './runtime.ts';
import { installedEngine } from './installed.ts';

/**
 * Creates an id for a new entity of type `t`. A pack's `#generated/ears` exports this
 * typed to brand the id with the entity type, so reads through its helpers infer the shape.
 *
 * @internal
 */
export const createEntity = <E extends EARS.Entity>(t: E): EARS.EntityId =>
  `${t}-${idSuffix()}` as EARS.EntityId;

const ENTITY_TYPE = 'entityType' as EARS.AttrKind;

/**
 * An entity's type, from its id: the one record of it. `createEntity` builds `<Type>-<suffix>`, `create`
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
  const store       = new Map<EARS.AttrKind, Map<EARS.EntityId, EARS.AttributeValue[]>>();
  const entityIndex = new Map<EARS.Entity, Set<EARS.EntityId>>();
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
    relations.clear();
  }

  const bucket = (k: EARS.AttrKind) => {
    if (!store.has(k)) store.set(k, new Map());
    return store.get(k)!;
  };

  /** Records how to put back the entity's values of a kind (a copy, since writes change the list in place) */
  const touchValues = (id: EARS.EntityId, kind: EARS.AttrKind) =>
    journal.open && journal.touch(`a\x1F${kind}\x1F${id}`, () => {
      const before = store.get(kind)?.get(id);
      const copy = before?.map((v) => (isPlainObject(v) ? { ...v } : v)) as EARS.AttributeValue[] | undefined;
      return () => {
        if (copy) bucket(kind).set(id, copy);
        else store.get(kind)?.delete(id);
      };
    });

  /** Records whether the entity was in the index, to put that back */
  const touchIndexed = (id: EARS.EntityId) =>
    journal.open && journal.touch(`e\x1F${id}`, () => {
      const type = entityTypeOf(id);
      const was = entityIndex.get(type)?.has(id) ?? false;
      return () => {
        const ids = entityIndex.get(type);
        if (was) {
          if (ids) ids.add(id);
          else entityIndex.set(type, new Set([id]));
        } else if (ids?.delete(id)) {
          removals++;
          if (ids.size === 0) entityIndex.delete(type);
        }
      };
    });

  /** Takes the entity out of the type index, and its type with it once it holds no other */
  const unindexEntity = (id: EARS.EntityId) => {
    const type = entityTypeOf(id);
    const ids = entityIndex.get(type);
    if (!ids?.has(id)) return;
    touchIndexed(id);
    ids.delete(id);
    if (ids.size === 0) entityIndex.delete(type);
  };

  const indexEntity = (id: EARS.EntityId) => {
    touchIndexed(id);
    const type = entityTypeOf(id);
    const ids = entityIndex.get(type);
    if (ids) ids.add(id);
    else entityIndex.set(type, new Set([id]));
  };

  /** The entity's values of a kind, created (and the entity indexed) on the first */
  const valuesOf = (id: EARS.EntityId, kind: EARS.AttrKind) => {
    const b = bucket(kind);
    let list = b.get(id);
    if (!list) {
      list = [];
      b.set(id, list);
      indexEntity(id);
    }
    return list;
  };

  /** Sets the value at `idx`, merging it into a plain object already there */
  const mergeAt = (list: EARS.AttributeValue[], val: unknown, idx: number) => {
    while (list.length < idx) list.push(null as unknown as EARS.AttributeValue);
    const cur = list[idx];
    list[idx] = cur && isPlainObject(cur) && isPlainObject(val) ? { ...cur, ...val } : val as EARS.AttributeValue;
  };

  const add = (id: EARS.EntityId, kind: EARS.AttrKind, val: unknown) => {
    touchValues(id, kind);
    const list = valuesOf(id, kind);
    list.push(val as EARS.AttributeValue);
    if (persisted(kind)) persistence.onPutAttrArray(kind, id, list);
  };

  const put = (id: EARS.EntityId, kind: EARS.AttrKind, val: unknown) => {
    touchValues(id, kind);
    bucket(kind).set(id, [val as EARS.AttributeValue]);
    indexEntity(id);
    if (persisted(kind)) persistence.onPutAttrArray(kind, id, [val]);
  };

  const merge = (id: EARS.EntityId, kind: EARS.AttrKind, val: unknown, idx = 0) => {
    touchValues(id, kind);
    const list = valuesOf(id, kind);
    mergeAt(list, val, idx);
    if (persisted(kind)) persistence.onPutAttrArray(kind, id, list);
  };

  const drop = (id: EARS.EntityId, kind: EARS.AttrKind, idx = 0) => {
    const list = bucket(kind).get(id);
    if (!list?.length) return;
    touchValues(id, kind);
    list.splice(idx, 1);
    if (!persisted(kind)) {
      if (!list.length) bucket(kind).delete(id);
    } else if (!list.length) {
      bucket(kind).delete(id);
      persistence.onDropAttr(kind, id, idx, []);
    } else {
      persistence.onPutAttrArray(kind, id, list);
    }
  };

  /** Removes every value of the kind */
  const deleteAttr = (id: EARS.EntityId, kind: EARS.AttrKind) => {
    if (!bucket(kind).get(id)?.length) return;
    touchValues(id, kind);
    bucket(kind).delete(id);
    if (persisted(kind)) persistence.onDropAttr(kind, id, 0, []);
  };

  const dropIf = (id: EARS.EntityId, kind: EARS.AttrKind, crit: unknown) => {
    const list = bucket(kind).get(id);
    if (!list) return;
    const i = list.findIndex(
      v =>
        v === crit ||
        (isPlainObject(v) &&
          isPlainObject(crit) &&
          Object.entries(crit).every(([k, v0]) => (v as any)[k] === v0)),
    );
    if (i !== -1) drop(id, kind, i);
  };

  /** A stored value, loaded without telling persistence */
  function bulkLoadAttr(id: EARS.EntityId, kind: EARS.AttrKind, val: unknown, idx = 0) {
    mergeAt(valuesOf(id, kind), val, idx);
  }

  const grantRole  = (id: EARS.EntityId, role: string) =>
    add(id, EARS.AttrKind.Role, role);
  const revokeRole = (id: EARS.EntityId, role: string) =>
    dropIf(id, EARS.AttrKind.Role, role);

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
    const relId = createEntity(EARS.Entity.Relation);
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
    // Before `d` changes in place below
    touchValues(relId, EARS.AttrKind.RelationDetails);
    const { sourceEntity: oS, targetEntity: oT, relationType: k } = d;
    if (newS) d.sourceEntity = newS;
    if (newT) d.targetEntity = newT;
    if (info !== undefined) d.info = info;
    merge(relId, EARS.AttrKind.RelationDetails, d);
    if (newS || newT)
      relations.updateIndex(k, relId, oS, oT, d.sourceEntity, d.targetEntity);
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
    drop(relId, EARS.AttrKind.RelationDetails);
    // A relation is its details: with them gone, so is the relation's entity
    unindexEntity(relId);
    if (destroyed) persistence.onRemoveRelation(relId, destroyed);
    else persistence.onRemoveRelation(relId);
  };
  const removeRelationById = (relId: EARS.EntityId) => removeRelation(relId);

  // `entityType` is read from the id rather than stored, so every row carries it and none can disagree
  const getAttr  = (id: EARS.EntityId, k: EARS.AttrKind, i = 0) =>
    k === ENTITY_TYPE ? (i === 0 && hasEntity(id) ? entityTypeOf(id) : null) : bucket(k).get(id)?.[i] ?? null;
  const getAttrs = (id: EARS.EntityId, k: EARS.AttrKind): EARS.AttributeValue[] =>
    k === ENTITY_TYPE ? (hasEntity(id) ? [entityTypeOf(id)] : []) : bucket(k).get(id) ?? [];
  const getRoles = (id: EARS.EntityId) =>
    getAttrs(id, EARS.AttrKind.Role) as string[];

  const getAll = (id: EARS.EntityId) => {
    const out: Record<string, unknown> = hasEntity(id) ? { [ENTITY_TYPE]: entityTypeOf(id) } : {};
    for (const [k, b] of store)
      if (b.get(id))
        out[k] = b.get(id)!.length === 1 ? b.get(id)![0] : b.get(id);
    return out;
  };

  const getAllEntities = () => {
    const all: EARS.EntityId[] = [];
    for (const set of entityIndex.values()) {
      for (const id of set) {
        all.push(id);
      }
    }
    return all;
  };

  const getEntitiesOfType = (t: EARS.Entity) =>
    [...(entityIndex.get(t) ?? [])];

  /** Whether the entity is in the engine, from its type's index rather than a scan of every entity */
  const hasEntity = (id: EARS.EntityId) =>
    entityIndex.get(entityTypeOf(id))?.has(id) ?? false;

  const queryEntitiesByRole = (role: string) =>
    getAllEntities().filter(id => getRoles(id).includes(role));

  const queryEntitiesByAttribute = (
    k: EARS.AttrKind,
    v?: unknown,
  ) =>
    v === undefined
      ? getAllEntities().filter(id => getAttrs(id, k).length)
      : getAllEntities().filter(id =>
          getAttrs(id, k).some(attr => attr === v),
        );

  const queryEntitiesInRelationTo = (target: EARS.EntityId) => {
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

  const queryEntitiesByRelationTo = (
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
      touchValues(id, k);
      b.delete(id);
    }
    unindexEntity(id);
    if (!skipPersistence) {
      persistence.onDestroyEntity(id);
    }
  }

  const getAllAttributeKinds = (): EARS.AttrKind[] => Array.from(store.keys());
  const getAllRelationKinds = (): string[] => Object.keys(relationIndex);
  const getAllEntityTypes = (): EARS.Entity[] => Array.from(entityIndex.keys());

  const getAttributeStats = (kind: EARS.AttrKind) => {
    const b = bucket(kind);
    let totalValues = 0;
    for (const values of b.values()) {
      totalValues += values.length;
    }
    return { entityCount: b.size, totalValues };
  };

  /** Tells the sink an entity was created */
  const entityCreated = (id: EARS.EntityId, entityType: EARS.Entity) => persistence.onCreateEntity(id, entityType);

  function getSchemaStats() {
    return {
      entities: getAllEntityTypes().reduce((acc: Record<string, number>, type: string) => {
        acc[type] = getEntitiesOfType(type as EARS.Entity).length;
        return acc;
      }, {}),
      attributes: getAllAttributeKinds().reduce((acc: Record<string, number>, kind: EARS.AttrKind) => {
        acc[kind as string] = getAttributeStats(kind).totalValues;
        return acc;
      }, {}),
      relations: Object.fromEntries(Object.entries(relationIndex).map(([kind, { bySource }]) =>
        [kind, Object.values(bySource).reduce((total, relIds) => total + relIds.length, 0)])),
    };
  }

  return {
    clear, bulkLoadAttr,
    putAttr: put, addAttr: add, mergeAttr: merge, dropAttr: drop, deleteAttr, dropIf, updateAttr: put,
    entityCreated, grantRole, revokeRole, addRelation, updateRelation, removeRelationById,
    getAttr, getAttrs, getRoles, getAll, getAllEntities, getEntitiesOfType, hasEntity, removals: () => removals,
    queryEntitiesByRole, queryEntitiesByAttribute, queryEntitiesInRelationTo, queryEntitiesByRelationTo,
    destroyEntity, getAllAttributeKinds, getAllRelationKinds, getAllEntityTypes, getAttributeStats, getSchemaStats,
  };
}

export type AttributeStorage = ReturnType<typeof createAttributeStorage>;

// The installed engine's reads and writes (see installed.ts)

export const grantRole  = (id: EARS.EntityId, role: string) =>
  installedEngine().grantRole(id, role);
export const revokeRole = (id: EARS.EntityId, role: string) =>
  installedEngine().revokeRole(id, role);

/** Removes one relation by its own id. See transaction-helpers.removeRelation
 *  for the source/kind/target form that pairs with createRelation. */
export const removeRelationById = (relId: EARS.EntityId) =>
  installedEngine().removeRelationById(relId);

export const getRoles = (id: EARS.EntityId) =>
  installedEngine().getRoles(id);

export const getAll = (id: EARS.EntityId) =>
  installedEngine().getAll(id);

export const getAllEntities = () =>
  installedEngine().getAllEntities();

export const getEntitiesOfType = (t: EARS.Entity) =>
  installedEngine().getEntitiesOfType(t);

export const queryEntitiesByAttribute = (
  k: EARS.AttrKind,
  v?: unknown,
) =>
  installedEngine().queryEntitiesByAttribute(k, v);

export const queryEntitiesInRelationTo = (target: EARS.EntityId) =>
  installedEngine().queryEntitiesInRelationTo(target);

export const queryEntitiesByRelationTo = (
  relKind: string,
  id: EARS.EntityId,
  asSource = false,
) =>
  installedEngine().queryEntitiesByRelationTo(relKind, id, asSource);

export function destroyEntity(id: EARS.EntityId, skipPersistence = false) {
  installedEngine().destroyEntity(id, skipPersistence);
}

export const getAllAttributeKinds = (): EARS.AttrKind[] => installedEngine().getAllAttributeKinds();
export const getAllRelationKinds = (): string[] => installedEngine().getAllRelationKinds();
export const getAllEntityTypes = (): EARS.Entity[] => installedEngine().getAllEntityTypes();

export const getAttributeStats = (kind: EARS.AttrKind) =>
  installedEngine().getAttributeStats(kind);

export function getSchemaStats() {
  return installedEngine().getSchemaStats();
}

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
export const createEntity = <E extends EARS.Entity>(t: E): EARS.EntityId =>
  `${t}-${idSuffix()}` as EARS.EntityId;

const ENTITY_TYPE = 'entityType' as EARS.AttrKind;

/** No value: an attribute an entity doesn't hold, which `null`, a value, can't say */
const ABSENT = Symbol('absent');

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
  // One value per attribute: a list is an array value. A stored value is never changed in place, so the value an
  // undo or a held sink call keeps is the value that was there
  const store       = new Map<EARS.AttrKind, Map<EARS.EntityId, EARS.AttributeValue>>();
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

  /** Memory's one write of an attribute: `next`, or nothing when it is `ABSENT`. Undo goes through it too */
  const place = (id: EARS.EntityId, kind: EARS.AttrKind, next: EARS.AttributeValue | typeof ABSENT) => {
    if (next === ABSENT) store.get(kind)?.delete(id);
    else bucket(kind).set(id, next);
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
    const type = entityTypeOf(id);
    const ids = entityIndex.get(type);
    if (ids?.has(id)) return;
    touchIndexed(id);
    if (ids) ids.add(id);
    else entityIndex.set(type, new Set([id]));
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

  /** Every attribute the entity holds, each exactly as it was written */
  const getAll = (id: EARS.EntityId) => {
    const out: Record<string, unknown> = hasEntity(id) ? { [ENTITY_TYPE]: entityTypeOf(id) } : {};
    for (const [k, b] of store) if (b.has(id)) out[k] = b.get(id);
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

  const findEntitiesByRole = (role: string) =>
    getAllEntities().filter(id => getRoles(id).includes(role));

  /** Entities holding the attribute, or, given a value, holding it or a list containing it */
  const findEntitiesByAttribute = (k: EARS.AttrKind, v?: unknown) =>
    getAllEntities().filter(id => v === undefined ? hasAttr(id, k) : holds(getAttr(id, k), v));

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
    putAttr: put, appendAttr: append, mergeAttr: merge, removeAt, removeValue, deleteAttr,
    entityCreated, grantRole, revokeRole, addRelation, updateRelation, removeRelationById,
    getAttr, hasAttr, getRoles, getAll, getAllEntities, getEntitiesOfType, hasEntity, removals: () => removals,
    findEntitiesByRole, findEntitiesByAttribute, findEntitiesInRelationTo, findEntitiesByRelationTo,
    destroyEntity, getAllAttributeKinds, getAllRelationKinds, getAllEntityTypes, getAttributeStats, getSchemaStats,
  };
}

export type AttributeStorage = ReturnType<typeof createAttributeStorage>;

/** Whether a stored value is `v`, or a list containing it: what `where(k, v)` matches */
export const holds = (value: unknown, v: unknown) =>
  value === v || (Array.isArray(value) && value.includes(v));

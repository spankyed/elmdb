import type { RelationIndexStore } from './relation-index.ts';
import { EARS } from './entities.ts';
import { idSuffix, isPlainObject, sameValue } from './utils.ts';
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

/**
 * One entity's fields, by kind: a plain object, which V8 lays out by shape, so rows written alike share one and a
 * row costs a fraction of a `Map`. A field is set with `setField`, since `__proto__` assigned would set the object's
 * prototype rather than hold a value
 */
type Row = Record<string, EARS.AttributeValue>;
const newRow = (): Row => ({});
const setField = (row: Row, kind: string, value: EARS.AttributeValue) => {
  if (kind === '__proto__') Object.defineProperty(row, kind, { value, writable: true, enumerable: true, configurable: true });
  else row[kind] = value;
};
const ownField = Object.prototype.hasOwnProperty;
const owns = (row: Row, kind: string) => ownField.call(row, kind);
const isEmpty = (row: Row) => {
  for (const _ in row) return false;
  return true;
};

/** An engine's attributes (by entity, then kind), its entity index by type, and the writes and reads over them */
export function createAttributeStorage({ relations, persistence, journal }: { relations: RelationIndexStore; persistence: PersistenceSink; journal: Journal }) {
  // One value per attribute: a list is an array value. A stored value is never changed in place, so the value an
  // undo or a held sink call keeps is the value that was there. Kept by entity, so a row is read from its own
  // fields; what is read by kind across entities is the value index, derived from these
  const rows        = new Map<EARS.EntityId, Row>();
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
    rows.clear();
    entityIndex.clear();
    trashed.clear();
    valueIndex.clear();
    relations.clear();
  }

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
      for (const [id, row] of rows) if (owns(row, kind)) indexValue(index, id, row[kind], true);
    }
    const held = index.get(value);
    return held === undefined ? none : typeof held === 'string' ? new Set([held]) : held;
  };

  /** Where the entity sits in its type's order */
  const orderOf = (id: EARS.EntityId) => entityIndex.get(entityTypeOf(id))?.get(id) ?? Infinity;

  /** Memory's one write of an attribute: `next`, or nothing when it is `ABSENT`. Undo goes through it too */
  const place = (id: EARS.EntityId, kind: EARS.AttrKind, next: EARS.AttributeValue | typeof ABSENT) => {
    let row = rows.get(id);
    if (valueIndex.size > 0) {
      const index = valueIndex.get(kind);
      if (index) {
        if (row && owns(row, kind)) indexValue(index, id, row[kind], false);
        if (next !== ABSENT) indexValue(index, id, next, true);
      }
    }
    if (next === ABSENT) {
      if (row && owns(row, kind)) {
        delete row[kind];
        if (isEmpty(row)) rows.delete(id);
      }
    } else {
      if (!row) rows.set(id, row = newRow());
      setField(row, kind, next);
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
      const before = current(id, kind);
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
    if (next === ABSENT && current(id, kind) === ABSENT) return;
    touchValue(id, kind);
    place(id, kind, next);
    if (next !== ABSENT) indexEntity(id);
    if (!persisted(kind)) return;
    if (next === ABSENT) persistence.onDropAttr(kind, id);
    else persistence.onPutAttr(kind, id, next);
  };

  const current = (id: EARS.EntityId, kind: EARS.AttrKind): EARS.AttributeValue | typeof ABSENT => {
    const row = rows.get(id);
    return row && owns(row, kind) ? row[kind] : ABSENT;
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
    assign(id, kind, [...listOf(id, kind, 'append'), val] as unknown as EARS.AttributeValue);

  /** Merges `val` into the attribute's object, or sets it when either isn't a plain object */
  const merge = (id: EARS.EntityId, kind: EARS.AttrKind, val: unknown) => {
    const cur = current(id, kind);
    const next = cur !== ABSENT && isPlainObject(cur) && isPlainObject(val) ? { ...cur, ...val } : val;
    assign(id, kind, next as EARS.AttributeValue);
  };

  /** Removes the list's element at `idx` */
  const removeAt = (id: EARS.EntityId, kind: EARS.AttrKind, idx: number) => {
    const list = listOf(id, kind, 'removeAt');
    if (idx < 0 || idx >= list.length) return;
    assign(id, kind, list.filter((_, i) => i !== idx) as unknown as EARS.AttributeValue);
  };

  /** Removes the list's first element equal to `crit` (for an object, the first holding each of its fields) */
  const removeValue = (id: EARS.EntityId, kind: EARS.AttrKind, crit: unknown) => {
    const list = listOf(id, kind, 'removeValue');
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
    source: EARS.EntityId,
    kind: string,
    target: EARS.EntityId,
    info?: unknown,
  ) {
    const entry = relationIndex[kind];
    if (entry?.bySource?.[source] && entry?.byTarget?.[target]) {
      const fromSource = new Set(entry.bySource[source]);
      const fromTarget = new Set(entry.byTarget[target]);
      for (const existingRelId of fromSource) {
        if (fromTarget.has(existingRelId)) {
          const existingRel = getAttr(existingRelId, EARS.AttrKind.RelationDetails) as EARS.RelationDetail;
          // The same edge: linking it again is a no-op, which keeps its id and its place among its siblings
          if (info === undefined || sameValue(existingRel.info, info)) return existingRelId;
        }
      }
    }
    const relId = newId(EARS.Entity.Relation);
    put(relId, EARS.AttrKind.RelationDetails, {
      source,
      target,
      kind,
      info,
    } as EARS.RelationDetail);
    relations.addToIndex(kind, source, target, relId);
    persistence.onAddRelation(relId, kind, source, target, info);
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
    const { source: oS, target: oT, kind: k } = d;
    const next: EARS.RelationDetail = {
      ...d,
      ...(newS && { source: newS }),
      ...(newT && { target: newT }),
      ...(info !== undefined && { info }),
    };
    put(relId, EARS.AttrKind.RelationDetails, next);
    if (newS || newT)
      relations.updateIndex(k, relId, oS, oT, next.source, next.target);
    const patch: any = {};
    if (newS) patch.source = newS;
    if (newT) patch.target = newT;
    if (info !== undefined) patch.info = info;
    persistence.onUpdateRelation(relId, patch);
  }

  /** Removes a relation from memory and tells the sink */
  const removeRelation = (relId: EARS.EntityId) => {
    const d = getAttr(
      relId,
      EARS.AttrKind.RelationDetails,
    ) as EARS.RelationDetail | null;
    if (d)
      relations.removeFromIndex(d.kind, d.source, d.target, relId);
    deleteAttr(relId, EARS.AttrKind.RelationDetails);
    // A relation is its details: with them gone, so is the relation's entity
    unindexEntity(relId);
    persistence.onRemoveRelation(relId);
  };

  // `entityType` is read from the id rather than stored, so every row carries it and none can disagree
  const getAttr = (id: EARS.EntityId, k: EARS.AttrKind): EARS.AttributeValue | null => {
    if (k === ENTITY_TYPE) return hasEntity(id) ? entityTypeOf(id) : null;
    const value = current(id, k);
    return value === ABSENT ? null : value;
  };
  /** Whether the entity holds the attribute, `null` included */
  const hasAttr = (id: EARS.EntityId, k: EARS.AttrKind) =>
    k === ENTITY_TYPE ? hasEntity(id) : current(id, k) !== ABSENT;
  const getRoles = (id: EARS.EntityId): string[] => {
    const roles = current(id, EARS.AttrKind.Role);
    return roles === ABSENT ? [] : roles as unknown as string[];
  };

  /** The entity's row — its id, type and every attribute, each exactly as written — or undefined when no entity has the id */
  const getRow = (id: EARS.EntityId): EntityRow | undefined => {
    if (!hasEntity(id)) return undefined;
    const row: EntityRow = { id, entityType: entityTypeOf(id) };
    const fields = rows.get(id);
    for (const k in fields) setField(row as Row, k, fields[k]);
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
  /**
   * Visits the type's entities in order, read as they are walked rather than copied first, until `visit` answers
   * false; whether it went to the end. A walk reaches only those in the type when it began: an entity entering while
   * it runs (a `where` predicate's write) takes a later place, where the walk stops.
   */
  const entitiesOfType = (t: EARS.Entity, visit: (id: EARS.EntityId) => boolean): boolean => {
    const bound = nextOrder;
    for (const [id, order] of entityIndex.get(t) ?? []) {
      if (order >= bound) return true;
      if (!visit(id)) return false;
    }
    return true;
  };

  /** Whether the entity is in the engine, from its type's index rather than a scan of every entity */
  const hasEntity = (id: EARS.EntityId) =>
    entityIndex.get(entityTypeOf(id))?.has(id) ?? false;
  /** Whether the entity is in the trash */
  const isTrashed = (id: EARS.EntityId) => trashed.has(id);
  /** Whether the entity is in the engine and not in the trash: what a read by type, id or link reaches */
  const isLive = (id: EARS.EntityId) => hasEntity(id) && !trashed.has(id);

  /** Every entity `id` has a relation to or from, of any kind */
  const findEntitiesInRelationTo = (id: EARS.EntityId) => {
    const out = new Set<EARS.EntityId>();
    for (const k of Object.keys(relationIndex)) {
      const { bySource, byTarget } = relationIndex[k];
      bySource[id]?.forEach(relId => {
        const { target } = getAttr(
          relId,
          EARS.AttrKind.RelationDetails,
        ) as EARS.RelationDetail;
        out.add(target);
      });
      byTarget[id]?.forEach(relId => {
        const { source } = getAttr(
          relId,
          EARS.AttrKind.RelationDetails,
        ) as EARS.RelationDetail;
        out.add(source);
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
        return asSource ? d.target : d.source;
      })
      .filter(Boolean);
  };

  /** The relations of a kind going out of `id` (or, with `outward` false, coming in), each a relation row */
  const relationsOf = (relKind: string, id: EARS.EntityId, outward: boolean) => {
    const dir = relationIndex[relKind];
    if (!dir) return [];
    return (outward ? dir.bySource[id] ?? [] : dir.byTarget[id] ?? [])
      .map((rel) => ({ ...(getAttr(rel, EARS.AttrKind.RelationDetails) as EARS.RelationDetail), id: rel }));
  };

  /** Deletes the entity and every edge to or from it, from memory and the sink */
  function destroyEntity(id: EARS.EntityId) {
    // A relation is its details, so destroying one is removing it, as an unlink of that one edge would
    if (entityTypeOf(id) === EARS.Entity.Relation) return removeRelation(id);
    removals++;
    for (const k of Object.keys(relationIndex)) {
      const { bySource, byTarget } = relationIndex[k];
      const relIds = [...(bySource[id] ?? []), ...(byTarget[id] ?? [])];
      for (const relId of relIds) removeRelation(relId);
      relations.forgetEntity(k, id);
    }
    for (const k of Object.keys(rows.get(id) ?? {}) as EARS.AttrKind[]) {
      touchValue(id, k);
      place(id, k, ABSENT);
    }
    unindexEntity(id);
    persistence.onDestroyEntity(id);
  }


  /** Tells the sink an entity was created */
  const entityCreated = (id: EARS.EntityId, entityType: EARS.Entity) => persistence.onCreateEntity(id, entityType);

  /**
   * The engine's data as it is, each map keyed by a name the data holds: entities per type, each attribute
   * kind's use, each relation kind's, and the entities holding each role
   */
  function getSchemaStats(): SchemaStats {
    const roles: Record<string, number> = {};
    const attributes: Record<string, { entityCount: number; totalValues: number }> = {};
    // One walk of the rows: each kind's holders and values, and the roles
    for (const row of rows.values()) {
      for (const [kind, value] of Object.entries(row)) {
        const stats = attributes[kind] ??= { entityCount: 0, totalValues: 0 };
        stats.entityCount++;
        stats.totalValues += Array.isArray(value) ? value.length : 1;
      }
      const held = owns(row, EARS.AttrKind.Role) ? row[EARS.AttrKind.Role] : undefined;
      if (held) for (const role of new Set(held as unknown as string[])) roles[role] = (roles[role] ?? 0) + 1;
    }
    return {
      entities: Object.fromEntries([...entityIndex].map(([type, ids]) => [type, ids.size])),
      attributes,
      relations: Object.fromEntries(Object.entries(relationIndex).map(([kind, { bySource, byTarget }]) => [kind, {
        total: Object.values(bySource).reduce((total, relIds) => total + relIds.length, 0),
        uniqueSources: Object.keys(bySource).length,
        uniqueTargets: Object.keys(byTarget).length,
      }])),
      roles,
    };
  }

  return {
    clear, bulkLoadAttr,
    putAttr: put, appendAttr: append, mergeAttr: merge, removeAt, removeValue, deleteAttr,
    entityCreated, grantRole, revokeRole, addRelation, updateRelation, removeRelationById: removeRelation,
    getAttr, hasAttr, getRoles, getRow, getAllEntities, getEntitiesOfType, entitiesOfType, hasEntity, isTrashed, isLive, holders, orderOf, removals: () => removals,
    findEntitiesInRelationTo, findEntitiesByRelationTo, relationsOf,
    destroyEntity, getSchemaStats,
  };
}

export type AttributeStorage = ReturnType<typeof createAttributeStorage>;

/** An entity as a read gives it: its id and type, and every attribute it holds */
export type EntityRow = { id: EARS.EntityId; entityType: EARS.Entity } & Record<string, unknown>;

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

/** Whether a stored value is `v`, or a list containing it: what `where(k, v)` matches */
export const holds = (value: unknown, v: unknown) =>
  value === v || (Array.isArray(value) && value.includes(v));

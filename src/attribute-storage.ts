import { EARS } from './entities.ts';
import { idSuffix, isPlainObject } from './utils.ts';
import type { Journal } from './journal.ts';
import type { PersistenceSink } from './runtime.ts';
import type { RelationStats } from './relations.ts';
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
export const setField = <V>(row: Record<string, V>, kind: string, value: V) => {
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
export function createAttributeStorage({ persistence, journal }: { persistence: PersistenceSink; journal: Journal }) {
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
   * The value indexes, per entity type and attribute kind: a value → the type's entities holding it, or holding a
   * list with it as an element (multikey). One is built from its type's rows on its first read (`holders`), which only
   * a declared index gets, then kept by `place`, the one write every change — a unit's undo, hydration, a destroy —
   * goes through. An object value isn't indexed.
   */
  // A value one entity holds keeps that id rather than a set of one: most values of a unique field (a title, a short
  // code) are held once, and a set per value is what a write would otherwise allocate
  const valueIndex = new Map<EARS.Entity, Map<EARS.AttrKind, Map<unknown, EARS.EntityId | Set<EARS.EntityId>>>>();
  // What `holders` answers for a value nobody holds; never written to, since only `indexValue` adds and it adds to its own
  const none: ReadonlySet<EARS.EntityId> = new Set();
  function clear() {
    if (journal.open) throw new Error('clear() inside transact(): emptying the engine is not a write a unit can undo');
    rows.clear();
    entityIndex.clear();
    trashed.clear();
    valueIndex.clear();
  }

  const indexKey = (index: Map<unknown, EARS.EntityId | Set<EARS.EntityId>>, id: EARS.EntityId, key: unknown, add: boolean) => {
    if (!isIndexKey(key)) return;
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

  /** The type's entities holding `value` under `kind`, or a list with it (a primitive: an object is never indexed) */
  const holders = (type: EARS.Entity, kind: EARS.AttrKind, value: unknown): ReadonlySet<EARS.EntityId> => {
    let byKind = valueIndex.get(type);
    if (!byKind) valueIndex.set(type, byKind = new Map());
    let index = byKind.get(kind);
    if (!index) {
      byKind.set(kind, index = new Map());
      for (const id of entityIndex.get(type)?.keys() ?? []) {
        const row = rows.get(id);
        if (row && owns(row, kind)) indexValue(index, id, row[kind], true);
      }
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
      const index = valueIndex.get(entityTypeOf(id))?.get(kind);
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
    else trashed.add(id);
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
        else leave(id);
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

  /**
   * Deletes the entity's attributes and its place in its type's index, from memory and the sink. Its relations are the
   * relations module's, removed before this
   */
  function destroyEntity(id: EARS.EntityId) {
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
   * The entities' data as it is, each map keyed by a name the data holds: entities per type, each attribute kind's use,
   * and the entities holding each role (each relation kind's use is the relations module's)
   */
  function entityStats(): Omit<SchemaStats, 'relations'> {
    // Maps while counting, since a field may be named for an Object.prototype member (`__proto__`, `constructor`)
    const roles = new Map<string, number>();
    const attributes = new Map<string, { entityCount: number; totalValues: number }>();
    // One walk of the rows: each kind's holders and values, and the roles
    for (const row of rows.values()) {
      for (const [kind, value] of Object.entries(row)) {
        let stats = attributes.get(kind);
        if (!stats) attributes.set(kind, stats = { entityCount: 0, totalValues: 0 });
        stats.entityCount++;
        stats.totalValues += Array.isArray(value) ? value.length : 1;
      }
      const held = owns(row, EARS.AttrKind.Role) ? row[EARS.AttrKind.Role] : undefined;
      if (held) for (const role of new Set(held as unknown as string[])) roles.set(role, (roles.get(role) ?? 0) + 1);
    }
    return {
      entities: Object.fromEntries([...entityIndex].map(([type, ids]) => [type, ids.size])),
      attributes: Object.fromEntries(attributes),
      roles: Object.fromEntries(roles),
    };
  }

  return {
    clear, bulkLoadAttr,
    putAttr: put, appendAttr: append, mergeAttr: merge, removeAt, removeValue, deleteAttr,
    entityCreated, grantRole, revokeRole,
    getAttr, hasAttr, getRoles, getRow, getAllEntities, getEntitiesOfType, entitiesOfType, hasEntity, isTrashed, isLive, holders, orderOf,
    destroyEntity, entityStats,
  };
}

export type AttributeStorage = ReturnType<typeof createAttributeStorage>;

/** An entity as a read gives it: its id and type, and every attribute it holds */
export type EntityRow = { id: EARS.EntityId; entityType: EARS.Entity } & Record<string, unknown>;

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

/**
 * Whether a value is one an index holds, a list's element or a whole value: `null`, a boolean, a finite number, or a
 * string up to 256 characters. Anything else — an object, a longer string — is found by filtering a type's rows
 */
export const isIndexKey = (value: unknown): boolean =>
  value === null || typeof value === 'boolean' || (typeof value === 'number' && Number.isFinite(value))
  || (typeof value === 'string' && value.length <= 256);

/** Whether a stored value is `v`, or a list containing it: what `where(k, v)` matches */
export const holds = (value: unknown, v: unknown) =>
  value === v || (Array.isArray(value) && value.includes(v));

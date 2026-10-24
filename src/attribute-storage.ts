import { EARS } from './entities.ts';
import { idSuffix, isPlainObject } from './utils.ts';
import type { RelationStats } from './relations.ts';
import type { Key, KvReader, KvWriter } from './kv.ts';
import type { StoreAccess } from './store-access.ts';
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

/**
 * An entity's type, from its id: the one record of it. `newId` builds `<Type>-<suffix>`, `create`
 * refuses an id naming another type, and the id is the wire's record too — a frontend opens an entity by
 * it, and reference nodes keep ids inside stored documents — so nothing else stores the type. The type's ranges are
 * keyed by this, and the `entityType` field every row carries is read from it.
 */
export const entityTypeOf = (id: EARS.EntityId) => {
  const dash = id.indexOf('-');
  return dash === -1 ? id as unknown as EARS.Entity : id.substring(0, dash) as EARS.Entity;
};

/**
 * Sets a field on an object a read builds: `__proto__` assigned would set the object's prototype rather than hold a
 * value, so it is defined as an own property
 */
export const setField = <V>(row: Record<string, V>, kind: string, value: V) => {
  if (kind === '__proto__') Object.defineProperty(row, kind, { value, writable: true, enumerable: true, configurable: true });
  else row[kind] = value;
};

/** What `['i', id]` holds: the entity's place in the store's sequence, and whether it is in the trash */
interface EntityRecord { seq: number; trashed: boolean }

/** Which of a type's entities a walk reaches */
export type TypeScope = 'live' | 'trashed' | 'all';

/**
 * An engine's entities and their attributes over its store (`store-access.ts`), in these keys:
 *
 * - `['v', id, kind]` → the attribute's value: an entity's fields are one range, by entity, so a row is read and a
 *   commit dirties them together
 * - `['i', id]` → `{ seq, trashed }`: that the entity exists, its place in the store's sequence, and the trash
 * - `['e', type, seq]` → id, a live entity; `['d', type, seq]` → id, a trashed one: a type's entities in the order
 *   they entered, the trash a range of its own, so a live walk reads no trashed entity
 * - `['c', type]` → how many live entities the type holds
 * - `['x', type, kind, value, seq]` → id: a type's index on a field, a list's elements each a key, kept once
 *   `['xb', type, kind]` records it built (on its first read, from the type's rows)
 * - `['s', key]` → the next number of a numbering sequence
 *
 * Every write runs in a unit (`access.write`): the one open, or one of its own.
 */
export function createAttributeStorage({ access, inUnit }: {
  access: StoreAccess;
  /** Whether a unit (`transact`) is open, which a clear refuses */
  inUnit: () => boolean;
}) {
  const { read, write } = access;

  // ─── Entities ──────────────────────────────────────────────────────────

  const recordOf = (r: KvReader, id: EARS.EntityId) => r.get(['i', id]) as EntityRecord | undefined;
  const typeKey = (trashed: boolean, type: EARS.Entity, seq: number): Key => [trashed ? 'd' : 'e', type, seq];

  /** Adds `by` to the type's live count */
  const count = (w: KvWriter, type: EARS.Entity, by: number) => {
    const next = ((w.get(['c', type]) as number | undefined) ?? 0) + by;
    if (next > 0) w.put(['c', type], next);
    else w.del(['c', type]);
  };

  /** Puts the entity in the store, last in its type's order, unless it is there */
  const ensureEntity = (w: KvWriter, id: EARS.EntityId) => {
    if (recordOf(w, id)) return;
    const type = entityTypeOf(id);
    const seq = access.nextSeq(w);
    w.put(['i', id], { seq, trashed: false } satisfies EntityRecord);
    w.put(typeKey(false, type, seq), id);
    count(w, type, 1);
  };

  /** Moves the entity between its type's live and trashed ranges */
  const setTrashed = (w: KvWriter, id: EARS.EntityId, trashed: boolean) => {
    const record = recordOf(w, id);
    if (!record || record.trashed === trashed) return;
    const type = entityTypeOf(id);
    w.del(typeKey(record.trashed, type, record.seq));
    w.put(typeKey(trashed, type, record.seq), id);
    w.put(['i', id], { seq: record.seq, trashed } satisfies EntityRecord);
    count(w, type, trashed ? -1 : 1);
  };

  // ─── Indexes ───────────────────────────────────────────────────────────

  /** The keys a value is filed under in an index: itself, or each element of a list, that an index holds */
  const indexKeysOf = (value: unknown): KeyPart[] => {
    if (value === undefined) return [];
    const elements = Array.isArray(value) ? [...new Set(value)] : [value];
    return elements.filter(isIndexKey) as KeyPart[];
  };
  type KeyPart = string | number | boolean | null;

  const isBuilt = (r: KvReader, type: EARS.Entity, kind: EARS.AttrKind) => r.get(['xb', type, kind]) !== undefined;

  /** Refiles the entity's value of a kind in the type's index, where the index is built */
  const reindex = (w: KvWriter, id: EARS.EntityId, kind: EARS.AttrKind, before: unknown, after: unknown) => {
    const type = entityTypeOf(id);
    if (!isBuilt(w, type, kind)) return;
    const seq = recordOf(w, id)?.seq;
    if (seq === undefined) return;
    const was = indexKeysOf(before);
    const now = indexKeysOf(after);
    for (const key of was) if (!now.includes(key)) w.del(['x', type, kind, key, seq]);
    for (const key of now) if (!was.includes(key)) w.put(['x', type, kind, key, seq], id);
  };

  /** Builds the type's index on a kind from its rows, the trash included, and records it built */
  const build = (type: EARS.Entity, kind: EARS.AttrKind) => write((w) => {
    if (isBuilt(w, type, kind)) return;
    for (const trashed of [false, true]) {
      const entities: Array<[number, EARS.EntityId]> = [];
      w.range({ prefix: [trashed ? 'd' : 'e', type] }, (key, id) => { entities.push([key[2] as number, id as EARS.EntityId]); });
      for (const [seq, id] of entities) {
        for (const key of indexKeysOf(w.get(['v', id, kind]))) w.put(['x', type, kind, key, seq], id);
      }
    }
    w.put(['xb', type, kind], true);
  });

  /**
   * The type's entities holding `value` under `kind`, or a list with it, the trash included, in the type's order: from
   * the index, which its first read builds. That read walks the type, as an index that isn't there yet is built in a
   * unit of its own, which a snapshot already open doesn't see
   */
  const holders = (type: EARS.Entity, kind: EARS.AttrKind, value: unknown): EARS.EntityId[] => read((r) => {
    if (isBuilt(r, type, kind)) {
      const found: Array<[number, EARS.EntityId]> = [];
      r.range({ prefix: ['x', type, kind, value as KeyPart] }, (key, id) => { found.push([key[4] as number, id as EARS.EntityId]); });
      return found.sort((a, b) => a[0] - b[0]).map(([, id]) => id);
    }
    const found: EARS.EntityId[] = [];
    entitiesOfType(type, (id) => {
      if (holds(r.get(['v', id, kind]), value)) found.push(id);
      return true;
    }, 'all');
    build(type, kind);
    return found;
  });

  // ─── Attribute writes ──────────────────────────────────────────────────

  /** Every write of an attribute: in its unit, and refiled in a built index */
  const assign = (id: EARS.EntityId, kind: EARS.AttrKind, next: unknown) => write((w) => {
    const key = ['v', id, kind];
    const before = w.get(key);
    if (next === undefined) {
      if (before === undefined) return;
      reindex(w, id, kind, before, undefined);
      w.del(key);
      if (kind === TRASHED_AT) setTrashed(w, id, false);
      return;
    }
    ensureEntity(w, id);
    reindex(w, id, kind, before, next);
    w.put(key, next);
    if (kind === TRASHED_AT) setTrashed(w, id, true);
  });

  /** The attribute's list, refusing a value that isn't one */
  const listOf = (r: KvReader, id: EARS.EntityId, kind: EARS.AttrKind, via: string): readonly unknown[] => {
    const value = r.get(['v', id, kind]);
    if (value === undefined) return [];
    if (Array.isArray(value)) return value;
    throw new EarsError('NOT_A_LIST', `${via}('${kind}'): ${id}'s value is not a list`);
  };

  const put = (id: EARS.EntityId, kind: EARS.AttrKind, val: unknown) => assign(id, kind, val);

  /** Appends `val` to the attribute's list, starting one when there is none */
  const append = (id: EARS.EntityId, kind: EARS.AttrKind, val: unknown) =>
    write((w) => assign(id, kind, [...listOf(w, id, kind, 'append'), val]));

  /** Merges `val` into the attribute's object, or sets it when either isn't a plain object */
  const merge = (id: EARS.EntityId, kind: EARS.AttrKind, val: unknown) => write((w) => {
    const cur = w.get(['v', id, kind]);
    assign(id, kind, cur !== undefined && isPlainObject(cur) && isPlainObject(val) ? { ...cur, ...val } : val);
  });

  /** Removes the list's element at `idx` */
  const removeAt = (id: EARS.EntityId, kind: EARS.AttrKind, idx: number) => write((w) => {
    const list = listOf(w, id, kind, 'removeAt');
    if (idx < 0 || idx >= list.length) return;
    assign(id, kind, list.filter((_, i) => i !== idx));
  });

  /** Removes the list's first element equal to `crit` (for an object, the first holding each of its fields) */
  const removeValue = (id: EARS.EntityId, kind: EARS.AttrKind, crit: unknown) => write((w) => {
    const list = listOf(w, id, kind, 'removeValue');
    const i = list.findIndex(
      v =>
        v === crit ||
        (isPlainObject(v) &&
          isPlainObject(crit) &&
          Object.entries(crit).every(([k, v0]) => (v as Record<string, unknown>)[k] === v0)),
    );
    if (i !== -1) assign(id, kind, list.filter((_, j) => j !== i));
  });

  /** Removes the attribute */
  const deleteAttr = (id: EARS.EntityId, kind: EARS.AttrKind) => assign(id, kind, undefined);

  /** A stored value, loaded as it was: no check, and no `updatedAt` stamp */
  const bulkLoadAttr = (id: EARS.EntityId, kind: EARS.AttrKind, val: unknown) => write((w) => {
    ensureEntity(w, id);
    reindex(w, id, kind, w.get(['v', id, kind]), val);
    w.put(['v', id, kind], val);
    if (kind === TRASHED_AT) setTrashed(w, id, true);
  });

  // Roles are one list; an entity with none holds no role attribute
  const grantRole = (id: EARS.EntityId, role: string) => write(() => {
    const roles = getRoles(id);
    if (!roles.includes(role)) assign(id, EARS.AttrKind.Role, [...roles, role]);
  });
  const revokeRole = (id: EARS.EntityId, role: string) => write(() => {
    const roles = getRoles(id);
    if (!roles.includes(role)) return;
    const rest = roles.filter((r) => r !== role);
    assign(id, EARS.AttrKind.Role, rest.length ? rest : undefined);
  });

  /**
   * Deletes the entity's attributes, their index entries and its place in its type's order. Its relations are the
   * relations module's, removed before this
   */
  const destroyEntity = (id: EARS.EntityId) => write((w) => {
    const fields: Array<[EARS.AttrKind, unknown]> = [];
    w.range({ prefix: ['v', id] }, (key, value) => { fields.push([key[2] as EARS.AttrKind, value]); });
    for (const [kind, value] of fields) {
      reindex(w, id, kind, value, undefined);
      w.del(['v', id, kind]);
    }
    const record = recordOf(w, id);
    if (record) {
      w.del(typeKey(record.trashed, entityTypeOf(id), record.seq));
      if (!record.trashed) count(w, entityTypeOf(id), -1);
      w.del(['i', id]);
    }
  });

  /** Empties the store */
  function clear() {
    if (inUnit()) throw new Error('clear() inside transact(): emptying the engine is not a write a unit can undo');
    write((w) => {
      const keys: Key[] = [];
      w.range({ values: false }, (key) => { keys.push(key); });
      for (const key of keys) w.del(key);
    });
  }

  // ─── Reads ─────────────────────────────────────────────────────────────

  // `entityType` is read from the id rather than stored, so every row carries it and none can disagree
  const getAttr = (id: EARS.EntityId, k: EARS.AttrKind): EARS.AttributeValue | null => read((r) => {
    if (k === ENTITY_TYPE) return recordOf(r, id) ? entityTypeOf(id) : null;
    const value = r.get(['v', id, k]);
    return value === undefined ? null : value as EARS.AttributeValue;
  });
  /** Whether the entity holds the attribute, `null` included */
  const hasAttr = (id: EARS.EntityId, k: EARS.AttrKind) => read((r) =>
    k === ENTITY_TYPE ? recordOf(r, id) !== undefined : r.get(['v', id, k]) !== undefined);
  const getRoles = (id: EARS.EntityId): string[] => read((r) => (r.get(['v', id, EARS.AttrKind.Role]) as string[] | undefined) ?? []);

  /** The entity's row — its id, type and every attribute, each exactly as written — or undefined when no entity has the id */
  const getRow = (id: EARS.EntityId): EntityRow | undefined => read((r) => {
    if (!recordOf(r, id)) return undefined;
    const row: EntityRow = { id, entityType: entityTypeOf(id) };
    r.range({ prefix: ['v', id] }, (key, value) => { setField(row as Record<string, unknown>, key[2] as string, value); });
    return row;
  });

  /**
   * Visits the type's entities in the order they entered — the live ones, the trashed ones, or both merged by their
   * place — until `visit` answers false; whether it went to the end. A walk reaches only those in the store when it
   * began: an entity a `where` predicate makes while it runs takes a later place, past where the walk stops.
   */
  const entitiesOfType = (t: EARS.Entity, visit: (id: EARS.EntityId) => boolean, scope: TypeScope = 'all'): boolean => read((r) => {
    const bound = access.lastSeq(r);
    if (scope !== 'all') {
      let ended = true;
      r.range({ prefix: [scope === 'live' ? 'e' : 'd', t] }, (key, id) => {
        if ((key[2] as number) > bound) return false;
        if (!visit(id as EARS.EntityId)) return (ended = false);
      });
      return ended;
    }
    // Both ranges, merged by place: the trash is collected, being the smaller, and the live range read as it goes
    const trashed: Array<[number, EARS.EntityId]> = [];
    r.range({ prefix: ['d', t] }, (key, id) => { trashed.push([key[2] as number, id as EARS.EntityId]); });
    let next = 0;
    let ended = true;
    r.range({ prefix: ['e', t] }, (key, id) => {
      const seq = key[2] as number;
      if (seq > bound) return false;
      while (next < trashed.length && trashed[next]![0] < seq) {
        if (!visit(trashed[next++]![1])) return (ended = false);
      }
      if (!visit(id as EARS.EntityId)) return (ended = false);
    });
    while (ended && next < trashed.length && trashed[next]![0] <= bound) {
      if (!visit(trashed[next++]![1])) ended = false;
    }
    return ended;
  });

  const getEntitiesOfType = (t: EARS.Entity): EARS.EntityId[] => {
    const ids: EARS.EntityId[] = [];
    entitiesOfType(t, (id) => (ids.push(id), true), 'all');
    return ids;
  };

  /** Every entity, in the order they entered, the trash included */
  const getAllEntities = (): EARS.EntityId[] => read((r) => {
    const all: Array<[number, EARS.EntityId]> = [];
    for (const range of ['e', 'd']) r.range({ prefix: [range] }, (key, id) => { all.push([key[2] as number, id as EARS.EntityId]); });
    return all.sort((a, b) => a[0] - b[0]).map(([, id]) => id);
  });

  /** How many entities each type holds, the trash included: a walk of the types' keys, decoding no value */
  const countEntities = (): Record<string, number> => read((r) => {
    const counts = new Map<string, number>();
    for (const range of ['e', 'd']) {
      r.range({ prefix: [range], values: false }, (key) => { counts.set(key[1] as string, (counts.get(key[1] as string) ?? 0) + 1); });
    }
    return Object.fromEntries(counts);
  });

  /** How many live entities the type holds */
  const countOfType = (t: EARS.Entity): number => read((r) => (r.get(['c', t]) as number | undefined) ?? 0);

  /** Whether the entity is in the engine */
  const hasEntity = (id: EARS.EntityId) => read((r) => recordOf(r, id) !== undefined);
  /** Whether the entity is in the trash */
  const isTrashed = (id: EARS.EntityId) => read((r) => recordOf(r, id)?.trashed === true);
  /** Whether the entity is in the engine and not in the trash: what a read by type, id or link reaches */
  const isLive = (id: EARS.EntityId) => read((r) => recordOf(r, id)?.trashed === false);
  /** Where the entity sits in the order entities entered */
  const orderOf = (id: EARS.EntityId) => read((r) => recordOf(r, id)?.seq ?? Infinity);

  // ─── Numbering ─────────────────────────────────────────────────────────

  /** The next number a numbering sequence hands out, or undefined before its first use */
  const getSequence = (key: string) => read((r) => r.get(['s', key]) as number | undefined);
  const setSequence = (key: string, next: number) => write((w) => w.put(['s', key], next));

  /**
   * The entities' data as it is, each map keyed by a name the data holds: entities per type, each attribute kind's use,
   * and the entities holding each role (each relation kind's use is the relations module's). An admin read: it walks
   * the store
   */
  const entityStats = (): Omit<SchemaStats, 'relations'> => read((r) => {
    // Maps while counting, since a field may be named for an Object.prototype member (`__proto__`, `constructor`)
    const roles = new Map<string, number>();
    const attributes = new Map<string, { entityCount: number; totalValues: number }>();
    r.range({ prefix: ['v'] }, (key, value) => {
      const kind = key[2] as string;
      let stats = attributes.get(kind);
      if (!stats) attributes.set(kind, stats = { entityCount: 0, totalValues: 0 });
      stats.entityCount++;
      stats.totalValues += Array.isArray(value) ? value.length : 1;
      if (kind === EARS.AttrKind.Role) for (const role of new Set(value as string[])) roles.set(role, (roles.get(role) ?? 0) + 1);
    });
    return { entities: countEntities(), attributes: Object.fromEntries(attributes), roles: Object.fromEntries(roles) };
  });

  return {
    clear, bulkLoadAttr,
    putAttr: put, appendAttr: append, mergeAttr: merge, removeAt, removeValue, deleteAttr,
    grantRole, revokeRole,
    getAttr, hasAttr, getRoles, getRow, getAllEntities, getEntitiesOfType, entitiesOfType, countOfType, countEntities, hasEntity, isTrashed,
    isLive, holders, orderOf, getSequence, setSequence,
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
 * string up to 256 characters with no NUL, which separates a key's elements. Anything else — an object, a longer
 * string — is found by filtering a type's rows
 */
export const isIndexKey = (value: unknown): boolean =>
  value === null || typeof value === 'boolean' || (typeof value === 'number' && Number.isFinite(value))
  || (typeof value === 'string' && value.length <= 256 && !value.includes('\u0000'));

/** Whether a stored value is `v`, or a list containing it: what `where(k, v)` matches */
export const holds = (value: unknown, v: unknown) =>
  value === v || (Array.isArray(value) && value.includes(v));

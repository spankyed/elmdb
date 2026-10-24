import { EARS } from './entities.ts';
import type { Journal } from './journal.ts';
import type { PersistenceSink, RelationEdge } from './runtime.ts';
import { RESERVED_ENTITY_TYPES } from './errors.ts';
import { idSuffix, sameValue } from './utils.ts';

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

/** What a relation's id starts with: a relation is no entity, but its id has an entity id's form, `<prefix>-<suffix>` */
const RELATION_ID = RESERVED_ENTITY_TYPES.relation;

/**
 * One kind's adjacency: the relations leaving each source and arriving at each target. A set keeps the order its ids
 * were added, so an entity's edges read in the order they were linked, and adds or removes one without walking the rest
 */
interface KindIndex {
  bySource: Map<EARS.EntityId, Set<EARS.RelationId>>;
  byTarget: Map<EARS.EntityId, Set<EARS.RelationId>>;
}
type Side = keyof KindIndex;

/**
 * An engine's relations: each one's row and the adjacency that finds it, with every write to either and every read
 * over them. One owner, so a relation's row and its ends change together. A relation is not an entity: it holds no
 * attributes and no type's index lists it. Each write records how to undo itself while a unit is open (`journal`) and
 * tells `persistence`.
 */
export function createRelations({ journal, persistence }: { journal: Journal; persistence: PersistenceSink }) {
  /** Each relation's row, by id. A row is frozen and replaced whole on a change, so a read hands out the stored row */
  const rows = new Map<EARS.RelationId, RelationRow>();
  const index = new Map<string, KindIndex>();
  // What a read answers for an entity with no relations of a kind; never written to
  const none: ReadonlySet<EARS.RelationId> = new Set();

  const rowOf = (id: EARS.RelationId, kind: string, source: EARS.EntityId, target: EARS.EntityId, info: unknown): RelationRow =>
    Object.freeze({ id, kind, source, target, info: info as EARS.AttributeValue | undefined });

  // ─── Undo ──────────────────────────────────────────────────────────────

  /** Records how to put back the relation's row */
  const touchRow = (relId: EARS.RelationId) =>
    journal.open && journal.touch(`rel\x1F${relId}`, () => {
      const before = rows.get(relId);
      return () => {
        if (before) rows.set(relId, before);
        else rows.delete(relId);
      };
    });

  /** Records how to put back one entity's list of one kind and side */
  const touchList = (kind: string, side: Side, entity: EARS.EntityId) =>
    journal.open && journal.touch(`r\x1F${kind}\x1F${side}\x1F${entity}`, () => {
      const before = index.get(kind)?.[side].get(entity);
      const copy = before && new Set(before);
      return () => {
        const entry = entryOf(kind);
        if (copy) entry[side].set(entity, copy);
        else entry[side].delete(entity);
        dropIfEmpty(kind, entry);
      };
    });

  // ─── The adjacency ─────────────────────────────────────────────────────

  const entryOf = (kind: string): KindIndex => {
    let entry = index.get(kind);
    if (!entry) index.set(kind, entry = { bySource: new Map(), byTarget: new Map() });
    return entry;
  };
  const dropIfEmpty = (kind: string, entry: KindIndex) => {
    if (entry.bySource.size === 0 && entry.byTarget.size === 0) index.delete(kind);
  };
  const listOf = (kind: string, side: Side, entity: EARS.EntityId): ReadonlySet<EARS.RelationId> =>
    index.get(kind)?.[side].get(entity) ?? none;

  const addToList = (entry: KindIndex, side: Side, entity: EARS.EntityId, relId: EARS.RelationId) => {
    const list = entry[side].get(entity);
    if (list) list.add(relId);
    else entry[side].set(entity, new Set([relId]));
  };
  const removeFromList = (entry: KindIndex, side: Side, entity: EARS.EntityId, relId: EARS.RelationId) => {
    const list = entry[side].get(entity);
    if (list?.delete(relId) && list.size === 0) entry[side].delete(entity);
  };

  const indexRelation = (kind: string, source: EARS.EntityId, target: EARS.EntityId, relId: EARS.RelationId) => {
    touchList(kind, 'bySource', source);
    touchList(kind, 'byTarget', target);
    const entry = entryOf(kind);
    addToList(entry, 'bySource', source, relId);
    addToList(entry, 'byTarget', target, relId);
  };
  const unindexRelation = (kind: string, source: EARS.EntityId, target: EARS.EntityId, relId: EARS.RelationId) => {
    const entry = index.get(kind);
    if (!entry) return;
    touchList(kind, 'bySource', source);
    touchList(kind, 'byTarget', target);
    removeFromList(entry, 'bySource', source, relId);
    removeFromList(entry, 'byTarget', target, relId);
    dropIfEmpty(kind, entry);
  };

  /** Puts the relation's row, recording how to put back the one before */
  const placeRow = (relId: EARS.RelationId, row: RelationRow | undefined) => {
    touchRow(relId);
    if (row) rows.set(relId, row);
    else rows.delete(relId);
  };

  // ─── Reads ─────────────────────────────────────────────────────────────

  /** The relation's row, or undefined when no relation has the id */
  const get = (relId: EARS.RelationId): RelationRow | undefined => rows.get(relId);
  /** Whether a relation has the id: any string, so a refusal of an id given untyped can say it names a relation */
  const has = (id: string) => rows.has(id as EARS.RelationId);

  /**
   * The relations both in an entity's set leaving it and in another's arriving at it — the edges between the two —
   * walking the smaller set, so a hub's thousands of edges are never walked to find the few between it and one entity.
   * Each set receives a relation's id when the relation is linked or moved, so the edges between two ends sit in the
   * same order in both, and either walk yields them in that order
   */
  function* between(leaving: ReadonlySet<EARS.RelationId>, arriving: ReadonlySet<EARS.RelationId>): Generator<EARS.RelationId> {
    const [walked, other] = leaving.size <= arriving.size ? [leaving, arriving] : [arriving, leaving];
    for (const id of walked) if (other.has(id)) yield id;
  }

  /** The ids of the relations matching every end and kind named; with none named, every relation */
  const matchIds = ({ kind, source, target }: Omit<RelationMatch, 'kind'> & { kind?: string }): EARS.RelationId[] => {
    const kinds = kind !== undefined ? [kind] : [...index.keys()];
    const out = new Set<EARS.RelationId>();
    for (const k of kinds) {
      const entry = index.get(k);
      if (!entry) continue;
      if (source !== undefined && target !== undefined) {
        const leaving = entry.bySource.get(source);
        const arriving = entry.byTarget.get(target);
        if (leaving && arriving) for (const id of between(leaving, arriving)) out.add(id);
      } else if (source !== undefined) {
        for (const id of entry.bySource.get(source) ?? none) out.add(id);
      } else if (target !== undefined) {
        for (const id of entry.byTarget.get(target) ?? none) out.add(id);
      } else {
        for (const ids of entry.bySource.values()) for (const id of ids) out.add(id);
      }
    }
    return [...out];
  };

  /** The rows of the matching relations */
  const find = (match: RelationMatch = {}): RelationRow[] => matchIds(match).map((id) => rows.get(id)!);

  /** The far ends of the kind's relations leaving `id` (or, with `outward` false, arriving at it) */
  const farEnds = (kind: string, id: EARS.EntityId, outward: boolean): EARS.EntityId[] => {
    const ends: EARS.EntityId[] = [];
    for (const relId of listOf(kind, outward ? 'bySource' : 'byTarget', id)) {
      const row = rows.get(relId)!;
      ends.push(outward ? row.target : row.source);
    }
    return ends;
  };

  /** The rows of the kind's relations leaving `id` (or, with `outward` false, arriving at it) */
  const edges = (kind: string, id: EARS.EntityId, outward: boolean): RelationRow[] =>
    Array.from(listOf(kind, outward ? 'bySource' : 'byTarget', id), (relId) => rows.get(relId)!);

  /** Every entity `id` has a relation to or from, of any kind */
  const linkedWith = (id: EARS.EntityId): EARS.EntityId[] => {
    const out = new Set<EARS.EntityId>();
    for (const { bySource, byTarget } of index.values()) {
      for (const relId of bySource.get(id) ?? none) out.add(rows.get(relId)!.target);
      for (const relId of byTarget.get(id) ?? none) out.add(rows.get(relId)!.source);
    }
    return [...out];
  };

  /** Per kind, its relations and the distinct entities at each end */
  const stats = (): Record<string, RelationStats> => Object.fromEntries([...index].map(([kind, { bySource, byTarget }]) => {
    let total = 0;
    for (const ids of bySource.values()) total += ids.size;
    return [kind, { total, uniqueSources: bySource.size, uniqueTargets: byTarget.size }];
  }));

  // ─── Writes ────────────────────────────────────────────────────────────

  /**
   * Links `source` to `target`: idempotent on the whole edge — kind, ends and `info`, with no `info` matching any edge
   * between the ends — so linking an edge again keeps its id and its place among its siblings. Returns the relation's id
   */
  function add(source: EARS.EntityId, kind: string, target: EARS.EntityId, info?: unknown): EARS.RelationId {
    const entry = index.get(kind);
    const leaving = entry?.bySource.get(source);
    const arriving = entry?.byTarget.get(target);
    if (leaving && arriving) {
      for (const existing of between(leaving, arriving)) {
        if (info === undefined || sameValue(rows.get(existing)?.info, info)) return existing;
      }
    }
    const relId = `${RELATION_ID}-${idSuffix()}` as EARS.RelationId;
    placeRow(relId, rowOf(relId, kind, source, target, info));
    indexRelation(kind, source, target, relId);
    persistence.onAddRelation(relId, kind, source, target, info);
    return relId;
  }

  /** Leaves one relation of the kind between the ends: removes those there, then links */
  function replace(source: EARS.EntityId, kind: string, target: EARS.EntityId, info?: unknown): EARS.RelationId {
    unlink({ source, kind, target });
    return add(source, kind, target, info);
  }

  /** Moves the relation to new ends, or replaces its info, keeping its id; an id no relation has is ignored */
  function update(relId: EARS.RelationId, source?: EARS.EntityId, target?: EARS.EntityId, info?: unknown): void {
    const before = rows.get(relId);
    if (!before) return;
    const next = rowOf(relId, before.kind, source || before.source, target || before.target, info !== undefined ? info : before.info);
    placeRow(relId, next);
    if (next.source !== before.source || next.target !== before.target) {
      unindexRelation(before.kind, before.source, before.target, relId);
      indexRelation(before.kind, next.source, next.target, relId);
    }
    const patch: { source?: EARS.EntityId; target?: EARS.EntityId; info?: unknown } = {};
    if (source) patch.source = source;
    if (target) patch.target = target;
    if (info !== undefined) patch.info = info;
    persistence.onUpdateRelation(relId, patch);
  }

  /** Removes the relation; an id no relation has is ignored */
  function remove(relId: EARS.RelationId): void {
    const row = rows.get(relId);
    if (!row) return;
    unindexRelation(row.kind, row.source, row.target, relId);
    placeRow(relId, undefined);
    persistence.onRemoveRelation(relId);
  }

  /** Removes the matching relations */
  const unlink = (match: Omit<RelationMatch, 'kind'> & { kind?: string }) => {
    for (const relId of matchIds(match)) remove(relId);
  };

  /** Removes every relation to or from `id`, and what is left of its lists: an entity being destroyed */
  function removeEntity(id: EARS.EntityId): void {
    for (const kind of index.keys()) {
      for (const relId of [...listOf(kind, 'bySource', id), ...listOf(kind, 'byTarget', id)]) remove(relId);
      const entry = index.get(kind);
      if (!entry) continue;
      touchList(kind, 'bySource', id);
      touchList(kind, 'byTarget', id);
      entry.bySource.delete(id);
      entry.byTarget.delete(id);
      dropIfEmpty(kind, entry);
    }
  }

  /** A stored relation, loaded without telling persistence */
  function bulkLoad(relId: EARS.RelationId, kind: string, source: EARS.EntityId, target: EARS.EntityId, info?: unknown): void {
    rows.set(relId, rowOf(relId, kind, source, target, info));
    indexRelation(kind, source, target, relId);
  }

  function clear(): void {
    rows.clear();
    index.clear();
  }

  return { get, has, find, farEnds, edges, linkedWith, stats, add, replace, update, remove, unlink, removeEntity, bulkLoad, clear };
}

export type Relations = ReturnType<typeof createRelations>;

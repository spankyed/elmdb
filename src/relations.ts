import { EARS } from './entities.ts';
import type { RelationEdge } from './runtime.ts';
import type { KvReader, KvWriter } from './kv.ts';
import type { StoreAccess } from './store-access.ts';
import { RESERVED_ENTITY_TYPES } from './errors.ts';
import { sameValue } from './utils.ts';

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

/** What `['r', relId]` holds: the relation's row, and its place in the store's sequence, which its edge keys carry */
interface StoredRelation { kind: string; source: EARS.EntityId; target: EARS.EntityId; info: unknown; seq: number }

/** An edge key's value: the relation, and the entity at its far end */
type Edge = [relId: EARS.RelationId, farEnd: EARS.EntityId];

const OUT = 'o';
const IN = 'n';
const BETWEEN = 'b';

/**
 * An engine's relations over its store, in these keys:
 *
 * - `['r', relId]` → the relation's row and its place
 * - `['o', source, kind, seq]` → `[relId, target]`, `['n', target, kind, seq]` → `[relId, source]`: each entity's edges
 *   out and in, by kind, in the order they were linked, so a hop reads the far end without the relation's row
 * - `['b', source, kind, target, seq]` → relId: the edges between two ends, so a link finds an edge it would repeat,
 *   and an unlink the ones it removes, without reading every edge of a hub
 *
 * A relation is not an entity: it holds no attributes and no type's range lists it. Each write runs in a unit
 * (`access.write`).
 */
export function createRelations({ access }: { access: StoreAccess }) {
  const { read, write } = access;

  const rowOf = (relId: EARS.RelationId, stored: StoredRelation): RelationRow =>
    ({ id: relId, kind: stored.kind, source: stored.source, target: stored.target, info: stored.info as EARS.AttributeValue | undefined });
  const storedOf = (r: KvReader, relId: EARS.RelationId) => r.get(['r', relId]) as StoredRelation | undefined;

  /** Visits the entity's edges out (or in) of the kind, or of every kind, in the order linked, until `visit` answers false */
  const edgesOf = (r: KvReader, entity: EARS.EntityId, outward: boolean, kind: string | undefined, visit: (edge: Edge) => boolean | void) =>
    r.range({ prefix: kind === undefined ? [outward ? OUT : IN, entity] : [outward ? OUT : IN, entity, kind] }, (_key, edge) => visit(edge as Edge));

  const fileEdges = (w: KvWriter, relId: EARS.RelationId, { kind, source, target, seq }: StoredRelation) => {
    w.put([OUT, source, kind, seq], [relId, target] satisfies Edge);
    w.put([IN, target, kind, seq], [relId, source] satisfies Edge);
    w.put([BETWEEN, source, kind, target, seq], relId);
  };
  const unfileEdges = (w: KvWriter, { kind, source, target, seq }: StoredRelation) => {
    w.del([OUT, source, kind, seq]);
    w.del([IN, target, kind, seq]);
    w.del([BETWEEN, source, kind, target, seq]);
  };

  /** Visits the relations of a kind from `source` to `target`, in the order linked, until `visit` answers false */
  const between = (r: KvReader, source: EARS.EntityId, kind: string, target: EARS.EntityId, visit: (relId: EARS.RelationId) => boolean | void) =>
    r.range({ prefix: [BETWEEN, source, kind, target] }, (_key, relId) => visit(relId as EARS.RelationId));

  // ─── Reads ─────────────────────────────────────────────────────────────

  /** The relation's row, or undefined when no relation has the id */
  const get = (relId: EARS.RelationId): RelationRow | undefined => read((r) => {
    const stored = storedOf(r, relId);
    return stored && rowOf(relId, stored);
  });
  /** Whether a relation has the id: any string, so a refusal of an id given untyped can say it names a relation */
  const has = (id: string) => read((r) => storedOf(r, id as EARS.RelationId) !== undefined);

  /** The rows of the matching relations: by an end, its edges in the order linked; by kind alone or none, every relation */
  const find = ({ kind, source, target }: Omit<RelationMatch, 'kind'> & { kind?: string } = {}): RelationRow[] => read((r) => {
    const rows: RelationRow[] = [];
    if (source !== undefined && target !== undefined && kind !== undefined) {
      between(r, source, kind, target, (relId) => { rows.push(rowOf(relId, storedOf(r, relId)!)); });
      return rows;
    }
    if (source !== undefined || target !== undefined) {
      const outward = source !== undefined;
      edgesOf(r, (outward ? source : target)!, outward, kind, ([relId, far]) => {
        if (outward && target !== undefined && far !== target) return;
        rows.push(rowOf(relId, storedOf(r, relId)!));
      });
      return rows;
    }
    r.range({ prefix: ['r'] }, (key, stored) => {
      if (kind === undefined || (stored as StoredRelation).kind === kind) rows.push(rowOf(key[1] as EARS.RelationId, stored as StoredRelation));
    });
    return rows;
  });

  /** The far ends of the kind's relations leaving `id` (or, with `outward` false, arriving at it) */
  const farEnds = (kind: string, id: EARS.EntityId, outward: boolean): EARS.EntityId[] => read((r) => {
    const ends: EARS.EntityId[] = [];
    edgesOf(r, id, outward, kind, ([, far]) => { ends.push(far); });
    return ends;
  });

  /** The rows of the kind's relations leaving `id` (or, with `outward` false, arriving at it) */
  const edges = (kind: string, id: EARS.EntityId, outward: boolean): RelationRow[] => read((r) => {
    const rows: RelationRow[] = [];
    edgesOf(r, id, outward, kind, ([relId]) => { rows.push(rowOf(relId, storedOf(r, relId)!)); });
    return rows;
  });

  /** Every entity `id` has a relation to or from, of any kind */
  const linkedWith = (id: EARS.EntityId): EARS.EntityId[] => read((r) => {
    const out = new Set<EARS.EntityId>();
    for (const outward of [true, false]) edgesOf(r, id, outward, undefined, ([, far]) => { out.add(far); });
    return [...out];
  });

  /** Per kind, its relations and the distinct entities at each end: an admin read, walking every relation */
  const stats = (): Record<string, RelationStats> => read((r) => {
    const kinds = new Map<string, { total: number; sources: Set<string>; targets: Set<string> }>();
    r.range({ prefix: ['r'] }, (_key, stored) => {
      const { kind, source, target } = stored as StoredRelation;
      let entry = kinds.get(kind);
      if (!entry) kinds.set(kind, entry = { total: 0, sources: new Set(), targets: new Set() });
      entry.total++;
      entry.sources.add(source);
      entry.targets.add(target);
    });
    return Object.fromEntries([...kinds].map(([kind, { total, sources, targets }]) => [kind, { total, uniqueSources: sources.size, uniqueTargets: targets.size }]));
  });

  // ─── Writes ────────────────────────────────────────────────────────────

  /** A new relation id: the time, then the store's sequence, so ids sort in the order relations were made */
  const newRelationId = (seq: number) =>
    `${RELATION_ID}-${Date.now().toString(36)}${seq.toString(36).padStart(8, '0')}` as EARS.RelationId;

  /**
   * Stores the relation and its edges: under the id given, refiled in its place when the store holds it, or under a new
   * id and place
   */
  const store = (w: KvWriter, relId: EARS.RelationId | undefined, kind: string, source: EARS.EntityId, target: EARS.EntityId, info: unknown) => {
    const held = relId && storedOf(w, relId);
    if (held) unfileEdges(w, held);
    const seq = held ? held.seq : access.nextSeq(w);
    const id = relId ?? newRelationId(seq);
    const stored: StoredRelation = { kind, source, target, info, seq };
    w.put(['r', id], stored);
    fileEdges(w, id, stored);
    return id;
  };

  /**
   * Links `source` to `target`: idempotent on the whole edge — kind, ends and `info`, with no `info` matching any edge
   * between the ends — so linking an edge again keeps its id and its place among its siblings. Returns the relation's id
   */
  const add = (source: EARS.EntityId, kind: string, target: EARS.EntityId, info?: unknown): EARS.RelationId => write((w) => {
    let existing: EARS.RelationId | undefined;
    between(w, source, kind, target, (relId) => {
      if (info === undefined || sameValue(storedOf(w, relId)?.info, info)) return ((existing = relId), false);
    });
    if (existing) return existing;
    return store(w, undefined, kind, source, target, info);
  });

  /** Removes the relation; an id no relation has is ignored */
  const remove = (relId: EARS.RelationId): void => write((w) => {
    const stored = storedOf(w, relId);
    if (!stored) return;
    unfileEdges(w, stored);
    w.del(['r', relId]);
  });

  /** Removes the matching relations */
  const unlink = (match: Omit<RelationMatch, 'kind'> & { kind?: string }) => write(() => {
    for (const { id } of find(match)) remove(id);
  });

  /** Leaves one relation of the kind between the ends: removes those there, then links */
  const replace = (source: EARS.EntityId, kind: string, target: EARS.EntityId, info?: unknown): EARS.RelationId => write(() => {
    unlink({ source, kind, target });
    return add(source, kind, target, info);
  });

  /**
   * Moves the relation to new ends, or replaces its info, keeping its id; an id no relation has is ignored. A move
   * files it last among its new ends' edges
   */
  const update = (relId: EARS.RelationId, source?: EARS.EntityId, target?: EARS.EntityId, info?: unknown): void => write((w) => {
    const before = storedOf(w, relId);
    if (!before) return;
    const next: StoredRelation = {
      kind: before.kind,
      source: source || before.source,
      target: target || before.target,
      info: info !== undefined ? info : before.info,
      seq: before.seq,
    };
    if (next.source !== before.source || next.target !== before.target) {
      unfileEdges(w, before);
      next.seq = access.nextSeq(w);
      fileEdges(w, relId, next);
    }
    w.put(['r', relId], next);
  });

  /** Removes every relation to or from `id`: an entity being destroyed */
  const removeEntity = (id: EARS.EntityId): void => write((w) => {
    const relIds = new Set<EARS.RelationId>();
    for (const outward of [true, false]) edgesOf(w, id, outward, undefined, ([relId]) => { relIds.add(relId); });
    for (const relId of relIds) remove(relId);
  });

  /** A stored relation, loaded under its own id, unchecked */
  const bulkLoad = (relId: EARS.RelationId, kind: string, source: EARS.EntityId, target: EARS.EntityId, info?: unknown): void =>
    write((w) => { store(w, relId, kind, source, target, info); });

  return { get, has, find, farEnds, edges, linkedWith, stats, add, replace, update, remove, unlink, removeEntity, bulkLoad };
}

export type Relations = ReturnType<typeof createRelations>;

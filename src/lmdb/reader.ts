/// <reference types="node" />
// Direct reads of an LMDB environment, without hydrating it (backups, db scripts)
import type { LmdbDbs } from './envs.ts';
import { isDeepStrictEqual } from 'node:util';

const US = '\x1F';

export type AttrRecord = { t: 'null' | 'string' | 'number' | 'boolean' | 'object' | 'array' | 'date' | 'blob'; v: any };
export type EntityMeta = { type: string; createdAt: number };
export type RelationRecord = { kind: string; src: string; tgt: string; info?: any; createdAt: number };

/**
 * Validate that a string is safe for use as a key component.
 * Prevents adversarial prefixes with US character (\x1F) validation.
 */
function assertKeySafe(s: string, label: string) {
  if (typeof s !== 'string' || s.length === 0 || s.includes(US)) {
    throw new Error(`[Query] Invalid ${label}: must be non-empty string without \\x1F character`);
  }
}

/**
 * Decode attribute record to its runtime value.
 * Handles Date deserialization and blob objects (stored as JSON).
 * Note: blob handling assumes JSON encoding stores objects like {b64:"..."}.
 * Update this when moving to binary encoding.
 */
export function decodeAttr(rec?: AttrRecord | null): unknown {
  if (!rec) return null;
  return rec.t === 'date' && typeof rec.v === 'string' ? new Date(rec.v) : rec.v;
}

/**
 * Smart equality comparison with support for Dates, ISO strings, and optional deep equality.
 */
function eq(a: unknown, b: unknown, deep = false): boolean {
  // Handle Date comparisons
  if (a instanceof Date && b instanceof Date) return a.getTime() === b.getTime();
  if (a instanceof Date && typeof b === 'number') return a.getTime() === b;
  if (b instanceof Date && typeof a === 'number') return b.getTime() === a;
  
  // Handle Date vs ISO string comparisons
  if (a instanceof Date && typeof b === 'string') {
    return a.toISOString() === b || a.getTime() === Date.parse(b);
  }
  if (b instanceof Date && typeof a === 'string') {
    return b.toISOString() === a || b.getTime() === Date.parse(a);
  }
  
  // Optional deep equality for objects/arrays
  if (deep && a && b && typeof a === 'object' && typeof b === 'object') {
    return isDeepStrictEqual(a, b);
  }
  
  return a === b;
}

/** An attribute's row: one per `(kind, entityId)` */
function attrKey(kind: string, entityId: string) {
  return `${kind}${US}${entityId}`;
}

export type FindByAttrOpts = {
  /** optional: only return entities of this type (uses the entities DB) */
  entityType?: string;
  /** return at most N unique entityIds */
  limit?: number;
  /** shortcut equality check instead of predicate: the value, or an element of a list value */
  equals?: unknown;
  /** opt-in deep equality for object/array comparisons */
  deepEquals?: boolean;
  /** custom predicate over the whole value (value, entityId) -> boolean */
  predicate?: (value: unknown, entityId: string) => boolean;
};

export class LmdbReader {
  constructor(private dbs: LmdbDbs) { }

  // ───────────────────────────── Attributes ─────────────────────────────

  /** An entity's attribute value as stored, or undefined when it holds none (a stored null reads as null) */
  getAttr(entityId: string, kind: string): unknown {
    assertKeySafe(entityId, 'entityId');
    assertKeySafe(kind, 'kind');
    const rec = this.dbs.attrs.get(attrKey(kind, entityId)) as AttrRecord | undefined;
    return rec === undefined ? undefined : decodeAttr(rec);
  }

  /** The entityIds holding the attribute, in key order */
  *entitiesHavingAttr(kind: string, limit = Infinity): Iterable<string> {
    assertKeySafe(kind, 'kind');
    let count = 0;
    const start = `${kind}${US}`, end = start + '\xFF';
    for (const { key } of this.dbs.attrs.getRange({ start, end })) {
      if (count++ >= limit) break;
      yield String(key).slice(start.length);
    }
  }

  /**
   * Finds entityIds by attribute content: scans the kind's rows, filtering by equals/predicate and (optionally)
   * entityType. O(#rows for kind).
   */
  findEntitiesByAttr(kind: string, opts: FindByAttrOpts = {}): string[] {
    return [...this.findEntitiesByAttrIter(kind, opts)];
  }

  /** `findEntitiesByAttr`, streamed for early exit */
  *findEntitiesByAttrIter(kind: string, opts: FindByAttrOpts = {}): Iterable<string> {
    assertKeySafe(kind, 'kind');
    const { entityType, equals, deepEquals = false, predicate, limit = Infinity } = opts;
    const start = `${kind}${US}`, end = start + '\xFF';
    let count = 0;

    for (const { key, value } of this.dbs.attrs.getRange({ start, end })) {
      if (count >= limit) break;
      const entityId = String(key).slice(start.length);
      if (entityType && (this.dbs.entities.get(entityId) as EntityMeta | undefined)?.type !== entityType) continue;

      const val = decodeAttr(value as AttrRecord);
      const pass = equals !== undefined ? eq(val, equals, deepEquals) || (Array.isArray(val) && val.some((v) => eq(v, equals, deepEquals)))
                 : predicate ? !!predicate(val, entityId)
                 : true;
      if (!pass) continue;
      yield entityId;
      count++;
    }
  }

  // ───────────────────────────── Entities ─────────────────────────────

  /** Iterate entityIds of a given type */
  *entitiesOfType(type: string): Iterable<string> {
    for (const { key: entityId, value: meta } of this.dbs.entities.getRange() as Iterable<{ key: string; value: EntityMeta }>) {
      if (meta.type === type) yield String(entityId);
    }
  }

  /** Get entity meta (type, timestamps) */
  getEntityMeta(entityId: string): EntityMeta | null {
    return (this.dbs.entities.get(entityId) as EntityMeta | undefined) ?? null;
  }

  // ───────────────────────────── Relations ─────────────────────────────

  /** 
   * Iterate relations meeting optional filters.
   * Note: O(R) fallback scan. Future optimization: add secondary indexes (relBySrc, relByTgt)
   * for O(1) prefix scans when neighbors/BFS become hot.
   */
  *relations(filter?: { kind?: string; src?: string; tgt?: string; limit?: number })
    : Iterable<{ relId: string; rel: RelationRecord }> {
    const { kind, src, tgt, limit = Infinity } = filter ?? {};
    let count = 0;
    
    for (const { key, value } of this.dbs.relations.getRange() as Iterable<{ key: string; value: RelationRecord }>) {
      const rel = value;
      if (kind && rel.kind !== kind) continue;
      if (src && rel.src !== src) continue;
      if (tgt && rel.tgt !== tgt) continue;
      yield { relId: String(key), rel };
      if (++count >= limit) break;
    }
  }

  /** 
   * Neighbors via relations: out / in / both.
   * Note: O(R) per call due to relation scan. Plan for secondary indexes if hot.
   */
  neighbors(id: string, opts?: { kind?: string; direction?: 'out' | 'in' | 'both' }): string[] {
    const kind = opts?.kind;
    const dir = opts?.direction ?? 'out';
    const set = new Set<string>();

    if (dir === 'out' || dir === 'both') {
      for (const { rel } of this.relations({ kind, src: id })) set.add(rel.tgt);
    }
    if (dir === 'in' || dir === 'both') {
      for (const { rel } of this.relations({ kind, tgt: id })) set.add(rel.src);
    }
    return [...set];
  }

  /**
   * Neighbors with edge metadata: returns edge information along with neighbor IDs.
   * Useful for graph operations needing relationship details.
   */
  neighborsWithEdges(id: string, opts?: { kind?: string; direction?: 'out' | 'in' | 'both' }): Array<{ from: string; to: string; kind: string; info?: any }> {
    const kind = opts?.kind;
    const dir = opts?.direction ?? 'out';
    const out: Array<{ from: string; to: string; kind: string; info?: any }> = [];

    if (dir === 'out' || dir === 'both') {
      for (const { rel } of this.relations({ kind, src: id })) {
        out.push({ from: rel.src, to: rel.tgt, kind: rel.kind, info: rel.info });
      }
    }
    if (dir === 'in' || dir === 'both') {
      for (const { rel } of this.relations({ kind, tgt: id })) {
        out.push({ from: rel.src, to: rel.tgt, kind: rel.kind, info: rel.info });
      }
    }
    return out;
  }

  /** 
   * Tiny BFS over relations.
   * Note: O(depth × R) due to neighbors() calls. Future: with secondary indexes, drops to O(edges on frontier).
   */
  bfs(startId: string, opts?: { maxDepth?: number; kind?: string; direction?: 'out' | 'in' | 'both' }) {
    const { maxDepth = 1, kind, direction = 'out' } = opts ?? {};
    const dist = new Map<string, number>([[startId, 0]]);
    const q: string[] = [startId];
    
    // Use index pointer instead of Array.shift() for O(1) queue operations
    for (let qi = 0; qi < q.length; qi++) {
      const cur = q[qi];
      const d = dist.get(cur)!;
      if (d >= maxDepth) continue;
      for (const n of this.neighbors(cur, { kind, direction })) {
        if (!dist.has(n)) {
          dist.set(n, d + 1);
          q.push(n);
        }
      }
    }
    return dist; // includes startId at distance 0
  }

  // ───────────────────────────── Generic range util ─────────────────────────────

  /**
   * Scan a raw key range from any DB.
   * Example: scan every row of the attribute `tags` with prefix "tags␟".
   */
  scan(dbName: keyof LmdbDbs, opts: { start: string; end: string; limit?: number; reverse?: boolean }) {
    const db = this.dbs[dbName] as any;
    return db.getRange(opts);
  }
}

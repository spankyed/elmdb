// Converts one environment written in an older storage format to the current one, in place, a step per format.
// Nothing in the app calls this: an older data dir is refused at open (`checkFormat`), and the conversion is an
// explicit, one-off operation on a copy (`scripts/ears-convert.ts`).
import * as fs from 'node:fs';
import * as path from 'node:path';
import { open, type Database, type RootDatabase } from 'lmdb';
import { entityTypeOf } from '../attribute-storage.ts';
import type { EARS } from '../entities.ts';
import { createEarsEngine } from '../engine.ts';
import { createOrderedKV } from '../kv.ts';
import { createLmdbKVBackend } from './kv-backend.ts';
import { LMDB_FORMAT_VERSION } from './store.ts';

/** An attribute row of formats 1 to 3: its value, with a top-level Date as an ISO string */
type AttrRecord = { t: 'null' | 'string' | 'number' | 'boolean' | 'object' | 'array' | 'date' | 'blob'; v: unknown };
/** A relation row of formats 1 to 3 */
type RelationRecord = { kind: string; src: string; tgt: string; info?: unknown; createdAt?: number };

/** An attribute row's value */
function decodeAttr(rec?: AttrRecord | null): unknown {
  if (!rec) return null;
  return rec.t === 'date' && typeof rec.v === 'string' ? new Date(rec.v) : rec.v;
}

const SEP = '\x1F';

/** What the conversion to format 2 removed and rewrote, per kind of row */
export interface FormatConversion {
  /** Attribute rows of `entityType`: the id says it */
  entityTypeAttrs: number;
  /** Attribute rows of `relationDetails`: the relations table holds each relation */
  relationDetailAttrs: number;
  /** Entity rows of relations: a relation is its row in the relations table */
  relationRows: number;
  /** Entity rows no attribute names: written for a relation's ends, or left by a relation that was removed */
  orphanRows: number;
  /** Entity rows rewritten to hold only their type, which is the id's */
  rewrittenRows: number;
  /** Entities with attributes and no row, given one */
  addedRows: number;
}

/**
 * Converts the format 1 environment at `basePath` to format 2, in one transaction, and records format 2.
 * Refuses an environment that records any format, since only one written before formats were recorded is
 * format 1.
 */
export function convertToFormat2(basePath: string): FormatConversion {
  return withEnv(basePath, 1, ({ root, meta }) => {
    const entities = root.openDB({ name: 'entities', encoding: 'json' }) as Database<Record<string, unknown>>;
    const attrs = root.openDB({ name: 'attrs', encoding: 'json' }) as Database<unknown>;

    const result: FormatConversion = { entityTypeAttrs: 0, relationDetailAttrs: 0, relationRows: 0, orphanRows: 0, rewrittenRows: 0, addedRows: 0 };
    root.transactionSync(() => {
      const withAttrs = new Set<string>();
      // Materialised, since the body removes keys from the range it walks
      for (const key of [...attrs.getKeys()] as string[]) {
        const kind = key.slice(0, key.indexOf(SEP));
        const id = key.slice(kind.length + 1, key.indexOf(SEP, kind.length + 1));
        if (kind === 'entityType') { attrs.remove(key); result.entityTypeAttrs++; }
        else if (kind === 'relationDetails') { attrs.remove(key); result.relationDetailAttrs++; }
        else withAttrs.add(id);
      }
      for (const { key, value } of [...entities.getRange()] as Array<{ key: string; value: Record<string, unknown> }>) {
        if (value?.type === 'Relation') { entities.remove(key); result.relationRows++; continue; }
        if (!withAttrs.has(key)) { entities.remove(key); result.orphanRows++; continue; }
        entities.put(key, { type: entityTypeOf(key as EARS.EntityId) });
        result.rewrittenRows++;
        withAttrs.delete(key);
      }
      for (const id of withAttrs) {
        entities.put(id, { type: entityTypeOf(id as EARS.EntityId) });
        result.addedRows++;
      }
      meta.put('format', 2);
    });
    return result;
  });
}

/** What the conversion to format 3 rewrote */
export interface Format3Conversion {
  /** Attributes stored as one row, rewritten under the key without an index */
  singleValues: number;
  /** Attributes stored as several rows (a role list, a list built by `add`), each joined into one array value */
  joinedLists: number;
  /** Entities marked `deleted: true`, put in the trash: `trashedAt`, from their `deletedAt` */
  trashed: number;
}

/**
 * Converts the format 2 environment at `basePath` to format 3, in one transaction, and records format 3: each
 * attribute's rows (`kind␟id␟index`) become one row (`kind␟id`). One row at index 0 is the value; several rows, or
 * a role, are one array by index, a hole read as null. The trash moves into the engine: an entity marked
 * `deleted: true` gets `trashedAt` (its `deletedAt`, else its `updatedAt`, else 0), and both marks go.
 */
export function convertToFormat3(basePath: string): Format3Conversion {
  return withEnv(basePath, 2, ({ root, meta }) => {
    const attrs = root.openDB({ name: 'attrs', encoding: 'json' }) as Database<AttrRecord>;
    const result: Format3Conversion = { singleValues: 0, joinedLists: 0, trashed: 0 };
    root.transactionSync(() => {
      const grouped = new Map<string, Array<[number, unknown]>>();
      // Materialised, since the body removes keys from the range it walks
      for (const { key, value } of [...attrs.getRange()] as Array<{ key: string; value: AttrRecord }>) {
        const last = key.lastIndexOf(SEP);
        const attribute = key.slice(0, last);
        let rows = grouped.get(attribute);
        if (!rows) grouped.set(attribute, rows = []);
        rows.push([Number(key.slice(last + 1)), decodeAttr(value)]);
        attrs.remove(key);
      }
      const valueOf = (kind: string, id: string) => grouped.get(`${kind}${SEP}${id}`)?.find(([index]) => index === 0)?.[1];
      for (const [attribute, rows] of grouped) {
        const kind = attribute.slice(0, attribute.indexOf(SEP));
        if (kind === 'deleted' || kind === 'deletedAt') {
          const id = attribute.slice(kind.length + 1);
          if (kind === 'deleted' && valueOf('deleted', id) === true) {
            const at = [valueOf('deletedAt', id), valueOf('updatedAt', id)].find((v) => typeof v === 'number') ?? 0;
            attrs.put(`trashedAt${SEP}${id}`, encode(at));
            result.trashed++;
          }
          continue;
        }
        let value: unknown;
        if (rows.length === 1 && rows[0][0] === 0 && kind !== 'role') {
          value = rows[0][1];
          result.singleValues++;
        } else {
          const list: unknown[] = [];
          for (const [index, element] of rows) list[index] = element;
          value = Array.from(list, (element) => element ?? null);
          result.joinedLists++;
        }
        attrs.put(attribute, encode(value));
      }
      meta.put('format', 3);
    });
    return result;
  });
}

/** What the conversion to format 4 loaded, and what it left behind */
/** Format 3 kept each numbering sequence as an entity of its own, `Sequence-<key>` */
const SEQUENCE_PREFIX = 'Sequence-';

export interface Format4Conversion {
  entities: number;
  attributes: number;
  relations: number;
  /** Attribute rows of an entity with no entity row, which format 3's hydration skipped too */
  orphanedAttributes: number;
  /** Relation rows naming no end or no kind, which format 3's hydration skipped too */
  invalidRelations: number;
  /** Numbering rows (`Sequence-<key>`), which format 4 keeps as sequences rather than entities */
  sequences: number;
  /** Attribute rows holding no value (`{ t: 'undefined' }`), which format 3's hydration read as absent and format 4 doesn't keep */
  emptyAttributes: number;
}

/**
 * Converts the format 3 environment at `basePath` to format 4, in place: its three tables are read as format 3's
 * hydration read them and loaded through an engine into the one database format 4 keeps (`kv`) — entities in the order
 * they were created, each relation under its id, and each numbering row (`Sequence-<key>`, holding the next number)
 * as the sequence it was — in one unit; the old tables are then emptied and format 4 recorded.
 */
export function convertToFormat4(basePath: string): Format4Conversion {
  return withEnv(basePath, 3, ({ root, meta }) => {
    const entityRows = root.openDB({ name: 'entities', encoding: 'json' }) as Database<{ type?: string }>;
    const attrRows = root.openDB({ name: 'attrs', encoding: 'json' }) as Database<AttrRecord>;
    const relationRows = root.openDB({ name: 'relations', encoding: 'json' }) as Database<RelationRecord>;
    const result: Format4Conversion = { entities: 0, attributes: 0, relations: 0, orphanedAttributes: 0, invalidRelations: 0, sequences: 0, emptyAttributes: 0 };

    const fields = new Map<string, Array<[string, unknown]>>();
    for (const key of entityRows.getKeys()) fields.set(String(key), []);
    for (const { key, value } of attrRows.getRange() as Iterable<{ key: string; value: AttrRecord }>) {
      const sep = key.indexOf(SEP);
      const row = fields.get(key.slice(sep + 1));
      if (!row) { result.orphanedAttributes++; continue; }
      const decoded = decodeAttr(value);
      if (decoded === undefined) { result.emptyAttributes++; continue; }
      row.push([key.slice(0, sep), decoded]);
    }
    const createdAt = (row: Array<[string, unknown]>) => {
      const at = row.find(([kind]) => kind === 'createdAt')?.[1];
      return typeof at === 'number' ? at : Infinity;
    };
    const sequences = new Map<string, unknown>();
    for (const [id, row] of fields) {
      if (!id.startsWith(SEQUENCE_PREFIX)) continue;
      sequences.set(id.slice(SEQUENCE_PREFIX.length), row.find(([kind]) => kind === 'next')?.[1]);
      fields.delete(id);
    }
    const entities = [...fields].filter(([, row]) => row.length > 0)
      .sort(([a, rowA], [b, rowB]) => createdAt(rowA) - createdAt(rowB) || (a < b ? -1 : 1));
    const relations: Array<[string, RelationRecord]> = [];
    for (const { key, value } of relationRows.getRange() as Iterable<{ key: string; value: RelationRecord }>) {
      if (!value?.src || !value.tgt || typeof value.kind !== 'string') { result.invalidRelations++; continue; }
      relations.push([String(key), value]);
    }
    relations.sort(([a, relA], [b, relB]) => (relA.createdAt ?? 0) - (relB.createdAt ?? 0) || (a < b ? -1 : 1));

    const db = root.openDB({ name: 'kv', keyEncoding: 'binary', encoding: 'binary', compression: false }) as Database<Uint8Array, Uint8Array>;
    const kv = createOrderedKV(createLmdbKVBackend(root, db, { committed() {}, synced() {}, durable: () => Promise.resolve() }));
    const { admin } = createEarsEngine({ kv, isEntityType: () => true, isRelationKind: () => true, isRole: () => true });
    admin.bulkLoad(() => {
      for (const [id, row] of entities) {
        for (const [kind, value] of row) admin.bulkLoadAttr(id as EARS.EntityId, kind as EARS.AttrKind, value);
        result.entities++;
        result.attributes += row.length;
      }
      for (const [relId, { kind, src, tgt, info }] of relations) {
        admin.bulkLoadRelation(relId as EARS.RelationId, kind, src as EARS.EntityId, tgt as EARS.EntityId, info ?? undefined);
        result.relations++;
      }
      // Where `numbering.ts` keeps a sequence
      kv.write((w) => {
        for (const [key, next] of sequences) if (typeof next === 'number') { w.put(['s', key], next); result.sequences++; }
      });
    });
    root.transactionSync(() => {
      // Collected first, since the body removes the keys a live cursor would walk
      for (const table of [entityRows, attrRows, relationRows]) for (const key of Array.from(table.getKeys())) table.remove(key);
      meta.put('format', 4);
    });
    return result;
  });
}

/** Each step a conversion took, from the format the environment recorded to the current one */
export type FormatSteps = Array<
  | { to: 2; result: FormatConversion }
  | { to: 3; result: Format3Conversion }
  | { to: 4; result: Format4Conversion }
>;

/** Converts the environment at `basePath` from the format it records to `LMDB_FORMAT_VERSION`, a step per format */
export function convertToCurrentFormat(basePath: string): FormatSteps {
  const steps: FormatSteps = [];
  let format = recordedFormat(basePath);
  if (format === LMDB_FORMAT_VERSION) return steps;
  if (format === 1) { steps.push({ to: 2, result: convertToFormat2(basePath) }); format = 2; }
  if (format === 2) { steps.push({ to: 3, result: convertToFormat3(basePath) }); format = 3; }
  if (format === 3) { steps.push({ to: 4, result: convertToFormat4(basePath) }); format = 4; }
  if (format !== LMDB_FORMAT_VERSION) throw new Error(`The database at ${basePath} records storage format ${JSON.stringify(format)}, which doesn't convert to ${LMDB_FORMAT_VERSION}`);
  return steps;
}

/** What compacting an environment did: its file's size before and after, in bytes */
export interface Compaction {
  before: number;
  after: number;
}

/**
 * Rewrites the environment at `basePath` without the pages its deletes freed, in place: LMDB never shrinks a file,
 * and a conversion writes the new format beside the old tables before it empties them, so a converted file holds
 * both. A compacting copy is written beside it and its `data.mdb` renamed over the original's, so the file is the
 * old one or the new one whatever happens part way. Run on a closed environment, as a conversion is.
 */
export async function compactEnvironment(basePath: string): Promise<Compaction> {
  const file = path.join(basePath, 'data.mdb');
  if (!fs.existsSync(file)) throw new Error(`No LMDB database at ${basePath}`);
  const before = fs.statSync(file).size;
  const beside = fs.mkdtempSync(`${basePath}.compacting-`);
  try {
    const root = open({ path: basePath, maxDbs: 8, readOnly: true });
    try {
      await root.backup(beside, true);
    } finally {
      root.close();
    }
    fs.renameSync(path.join(beside, 'data.mdb'), file);
  } finally {
    fs.rmSync(beside, { recursive: true, force: true });
  }
  return { before, after: fs.statSync(file).size };
}

/** The format an environment records, 1 when it records none */
function recordedFormat(basePath: string): unknown {
  return withEnv(basePath, undefined, ({ meta }) => meta.get('format') ?? 1);
}

/**
 * Opens the environment, refusing another kind's or one that records a format other than `from` (when given), and
 * closes it after
 */
function withEnv<T>(basePath: string, from: number | undefined, fn: (env: { root: RootDatabase; meta: Database<unknown> }) => T): T {
  if (!fs.existsSync(path.join(basePath, 'data.mdb'))) throw new Error(`No LMDB database at ${basePath}`);
  const root = open({ path: basePath, maxDbs: 8, compression: true });
  try {
    const meta = root.openDB({ name: 'meta', encoding: 'json' }) as Database<unknown>;
    // Another kind of environment (a record store) records its own format, which no step here converts
    const kind = meta.get('kind');
    if (kind !== undefined) throw new Error(`The files at ${basePath} are a ${JSON.stringify(kind)} store, not an EARS database`);
    const recorded = meta.get('format') ?? 1;
    if (from !== undefined && recorded !== from) {
      throw new Error(`The database at ${basePath} records storage format ${JSON.stringify(recorded)}; this step converts format ${from}`);
    }
    return fn({ root, meta });
  } finally {
    root.close();
  }
}

/** An attribute record of formats 1 to 3 */
function encode(value: unknown): AttrRecord {
  if (value instanceof Date) return { t: 'date', v: value.toISOString() };
  const t = value === null ? 'null' : Array.isArray(value) ? 'array' : typeof value === 'object' ? 'object' : typeof value;
  return { t: t as AttrRecord['t'], v: value };
}

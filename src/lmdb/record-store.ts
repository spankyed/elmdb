/// <reference types="node" />
import { open, type Database, type RootDatabase } from 'lmdb';
import * as fs from 'node:fs';
import * as path from 'node:path';

/**
 * How this version encodes a record store's records and index entries, stamped in its `meta` database under `format`
 * as an EARS environment stamps `LMDB_FORMAT_VERSION`. Beside it, `kind` says the files are a record store
 * (`RECORD_STORE_KIND`), which is what tells the two apart: each refuses the other's files.
 */
export const RECORD_STORE_FORMAT_VERSION = 1;
export const RECORD_STORE_KIND = 'records';

const META_DB = 'meta';
const FORMAT_KEY = 'format';
const KIND_KEY = 'kind';
/** Sorts after every id, so a range over one index value ends after its last entry */
const ID_CEILING = '￿';

/** A record a store holds: its id and any fields, the indexed ones primitive */
export type StoredRecord = { id: string } & Record<string, unknown>;

/** How `findBy` walks one index value's records: oldest first unless `reverse` */
export interface FindByOptions {
  /** Newest first */
  reverse?: boolean;
  limit?: number;
  /** An id the walk resumes after, in its direction: the last id of the page before */
  cursor?: string;
}

export interface RecordStoreOptions {
  /** The environment's directory */
  path: string;
  /** The fields `findBy` looks records up by; a record whose value for one is not a primitive isn't indexed under it */
  indexes: readonly string[];
  readOnly?: boolean;
}

/**
 * Whole records by id in their own LMDB environment, with an index per declared field. Each write is one
 * transaction (so a read sees it at once) without a sync per write: a crash of the machine, not of the process, can
 * lose the last writes, which suits what a record store is for — a log that can afford them, such as run history.
 */
export interface RecordStore<R extends StoredRecord = StoredRecord> {
  readonly path: string;
  readonly readOnly: boolean;
  /** The record with this id, or undefined */
  get(id: string): R | undefined;
  /** Writes the whole record, replacing the one with its id and its index entries */
  put(record: R): void;
  /** Removes the record and its index entries; an id with no record is ignored */
  delete(id: string): void;
  /** The records whose `index` field holds `value`, in id order — creation order, for ids from `newId` */
  findBy(index: string, value: string | number | boolean, options?: FindByOptions): R[];
  /** A new id, `<prefix>-<n>`, sorting after every id this store handed out before (a same-millisecond one too) */
  newId(prefix: string): string;
  close(): void;
  reopen(): void;
  /** Deletes the files and opens the store empty */
  reset(): Promise<void>;
  /** LMDB's own copy of one read transaction into `targetDir/data.mdb` */
  copyTo(targetDir: string): Promise<void>;
}

/** An index entry's key: the field, its value and the record's id, so one value's entries sort by id */
type IndexKey = [string, string | number | boolean, string];

type Dbs = { root: RootDatabase; records: Database<StoredRecord, string>; index: Database<1, IndexKey> };

const isIndexable = (value: unknown): value is string | number | boolean =>
  typeof value === 'string' || typeof value === 'boolean' || (typeof value === 'number' && !Number.isNaN(value));

/** Opens (creating it, unless read-only) the record store at `path`; refuses files in another format */
export function openRecordStore<R extends StoredRecord = StoredRecord>({ path: dir, indexes, readOnly = false }: RecordStoreOptions): RecordStore<R> {
  let dbs: Dbs | null = null;
  // Time-sortable ids: the clock in base 36, then a counter for ids handed out in the same millisecond
  let lastTime = 0;
  let sameTime = 0;

  function openDbs(): Dbs {
    if (readOnly && !fs.existsSync(path.join(dir, 'data.mdb'))) throw new Error(`No record store at ${dir}`);
    if (!readOnly) fs.mkdirSync(path.dirname(dir), { recursive: true });
    const root = open({ path: dir, maxDbs: 4, noSync: true, readOnly });
    try {
      const meta = root.openDB({ name: META_DB, encoding: 'json' }) as Database<unknown> | undefined;
      const format = meta?.get(FORMAT_KEY);
      if (format === undefined) {
        // A new, empty store is stamped before anything is written to it
        if (!readOnly && meta) meta.transactionSync(() => {
          meta.putSync(KIND_KEY, RECORD_STORE_KIND);
          meta.putSync(FORMAT_KEY, RECORD_STORE_FORMAT_VERSION);
        });
      } else if (meta?.get(KIND_KEY) !== RECORD_STORE_KIND) {
        throw new Error(`The files at ${dir} are an EARS database, not a record store`);
      } else if (format !== RECORD_STORE_FORMAT_VERSION) {
        const newer = typeof format === 'number' && format > RECORD_STORE_FORMAT_VERSION;
        throw new Error(
          `The record store at ${dir} is in format ${JSON.stringify(format)}, but this version reads format ${RECORD_STORE_FORMAT_VERSION}`
          + (newer ? ': it was written by a newer apack, so update this one' : ''),
        );
      }
      return {
        root,
        records: root.openDB({ name: 'records', encoding: 'json' }),
        index: root.openDB({ name: 'index', encoding: 'json' }) as unknown as Database<1, IndexKey>,
      };
    } catch (error) {
      root.close();
      throw error;
    }
  }

  function current(): Dbs {
    if (!dbs) throw new Error(`The record store at ${dir} is closed`);
    return dbs;
  }

  function writable(): Dbs {
    if (readOnly) throw new Error(`The record store at ${dir} is open read-only`);
    return current();
  }

  const entries = (record: StoredRecord) =>
    indexes.filter((field) => isIndexable(record[field])).map((field): IndexKey => [field, record[field] as IndexKey[1], record.id]);

  function close(): void {
    if (!dbs) return;
    dbs.root.close();
    dbs = null;
  }

  dbs = openDbs();

  return {
    path: dir,
    readOnly,
    get: (id) => current().records.get(id) as R | undefined,
    put(record) {
      const { root, records, index } = writable();
      root.transactionSync(() => {
        const previous = records.get(record.id);
        if (previous) for (const key of entries(previous)) index.removeSync(key);
        records.putSync(record.id, record);
        for (const key of entries(record)) index.putSync(key, 1);
      });
    },
    delete(id) {
      const { root, records, index } = writable();
      root.transactionSync(() => {
        const previous = records.get(id);
        if (!previous) return;
        for (const key of entries(previous)) index.removeSync(key);
        records.removeSync(id);
      });
    },
    findBy(field, value, { reverse = false, limit, cursor } = {}) {
      const { records, index } = current();
      // A reverse range starts at its high bound; a cursor bounds the end the walk starts from
      const low: IndexKey = [field, value, !reverse && cursor !== undefined ? cursor : ''];
      const high: IndexKey = [field, value, reverse && cursor !== undefined ? cursor : ID_CEILING];
      const range = index.getKeys(reverse ? { start: high, end: low, reverse: true } : { start: low, end: high });
      const found: R[] = [];
      // `for…of` closes the range's read transaction however the loop ends
      for (const key of range) {
        const id = key[2];
        if (id === cursor) continue;
        const record = records.get(id) as R | undefined;
        if (record) found.push(record);
        if (limit !== undefined && found.length >= limit) break;
      }
      return found;
    },
    newId(prefix) {
      const now = Date.now();
      if (now > lastTime) { lastTime = now; sameTime = 0; } else sameTime++;
      return `${prefix}-${lastTime.toString(36).padStart(9, '0')}${sameTime.toString(36).padStart(4, '0')}`;
    },
    close,
    reopen() {
      close();
      dbs = openDbs();
    },
    async reset() {
      if (readOnly) throw new Error(`The record store at ${dir} is open read-only`);
      close();
      // Let LMDB release the files before deleting them
      await new Promise((resolve) => setTimeout(resolve, 100));
      fs.rmSync(dir, { recursive: true, force: true });
      dbs = openDbs();
    },
    async copyTo(targetDir) {
      fs.mkdirSync(targetDir, { recursive: true });
      await current().root.backup(targetDir, false);
    },
  };
}

/// <reference types="node" />
import { open, type Database, type RootDatabase } from 'lmdb';
import * as fs from 'node:fs';
import * as path from 'node:path';
import { _idClock, _indexEntries, type IndexValue, type RecordIndexes, type RecordStore, type StoredRecord } from '../records.ts';
import { checkStamp } from './stamp.ts';

/**
 * How this version encodes a record store's records and index entries, stamped in its `meta` database under `format`
 * as an EARS environment stamps `LMDB_FORMAT_VERSION`. Beside it, `kind` says the files are a record store
 * (`RECORD_STORE_KIND`), which is what tells the two apart: each refuses the other's files.
 */
export const RECORD_STORE_FORMAT_VERSION = 1;
export const RECORD_STORE_KIND = 'records';

/** Sorts after every id, so a range over one index value ends after its last entry */
const ID_CEILING = '\uffff';

export interface RecordStoreOptions<R extends { id: string } = StoredRecord> {
  /** The environment's directory */
  path: string;
  /** The indexes `findBy` looks records up by (`RecordIndexes`) */
  indexes: RecordIndexes<R>;
  readOnly?: boolean;
}

/** An index entry's key: the index, the value filed under it and the record's id, so one value's entries sort by id */
type IndexKey = [string, IndexValue, string];

type Dbs = { root: RootDatabase; records: Database<StoredRecord, string>; index: Database<1, IndexKey> };

/**
 * Opens (creating it, unless read-only) the record store at `path`, whole records by id in their own LMDB environment
 * with an index per declared name; refuses files of another kind or format. Each write is one transaction (so a read
 * sees it at once) without a sync per write: a crash of the machine, not of the process, can lose the last writes,
 * which suits what a record store is for — a log that can afford them, such as run history.
 */
export function openRecordStore<R extends { id: string } = StoredRecord>({ path: dir, indexes, readOnly = false }: RecordStoreOptions<R>): RecordStore<R> {
  let dbs: Dbs | null = null;
  const newId = _idClock();

  function openDbs(): Dbs {
    if (readOnly && !fs.existsSync(path.join(dir, 'data.mdb'))) throw new Error(`No record store at ${dir}`);
    if (!readOnly) fs.mkdirSync(path.dirname(dir), { recursive: true });
    const root = open({ path: dir, maxDbs: 4, noSync: true, readOnly });
    try {
      checkStamp(root, dir, readOnly, { kind: RECORD_STORE_KIND, format: RECORD_STORE_FORMAT_VERSION, noun: 'record store' });
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

  const entries = (record: StoredRecord): IndexKey[] =>
    _indexEntries(indexes, record as R).map(([name, value]) => [name, value, record.id]);

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
    findBy(name, value, { reverse = false, limit, cursor } = {}) {
      const { records, index } = current();
      // A reverse range starts at its high bound; a cursor bounds the end the walk starts from
      const low: IndexKey = [name, value, !reverse && cursor !== undefined ? cursor : ''];
      const high: IndexKey = [name, value, reverse && cursor !== undefined ? cursor : ID_CEILING];
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
    newId,
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

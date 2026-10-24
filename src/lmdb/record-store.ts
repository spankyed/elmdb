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

/** A record store's environment at `dir`, checked to be one in this version's format */
function openDbs(dir: string, readOnly: boolean): Dbs {
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

/**
 * Opens (creating it, unless read-only) the record store at `path`, whole records by id in their own LMDB environment
 * with an index per declared name; refuses files of another kind or format. Each write is one transaction (so a read
 * sees it at once) without a sync per write: a crash of the machine, not of the process, can lose the last writes,
 * which suits what a record store is for — a log that can afford them, such as run history.
 */
export function openRecordStore<R extends { id: string } = StoredRecord>({ path: dir, indexes, readOnly = false }: RecordStoreOptions<R>): RecordStore<R> {
  let dbs: Dbs | null = null;
  /**
   * Why opening the files again failed, while it has: the store has nowhere to write, so a write throws rather than
   * being dropped as it is while files are being replaced
   */
  let openFailure: Error | null = null;
  const newId = _idClock();

  function current(): Dbs {
    if (!dbs) throw new Error(`The record store at ${dir} is closed`);
    return dbs;
  }

  /**
   * The open environment to write to, or nothing while the store is closed — for a reset or an import, which
   * replace it — when the write is dropped: a record store is a log, and what is written while its files are being
   * replaced has nowhere to go, as the LMDB store drops writes made while it is closed
   */
  function writable(): Dbs | null {
    if (readOnly) throw new Error(`The record store at ${dir} is open read-only`);
    if (openFailure) {
      throw new Error(
        `The record store at ${dir} is closed: opening it again failed (${openFailure.message}). `
        + 'Nothing can be saved until the app is started again.',
      );
    }
    return dbs;
  }

  /** Opens the files again, remembering a failure for the writes that follow */
  function reopenDbs(): void {
    try {
      dbs = openDbs(dir, readOnly);
      openFailure = null;
    } catch (error) {
      openFailure = error instanceof Error ? error : new Error(String(error));
      throw error;
    }
  }

  const entries = (record: StoredRecord): IndexKey[] =>
    _indexEntries(indexes, record as R).map(([name, value]) => [name, value, record.id]);

  function close(): void {
    if (!dbs) return;
    dbs.root.close();
    dbs = null;
  }

  dbs = openDbs(dir, readOnly);

  return {
    path: dir,
    readOnly,
    // While closed, a read finds nothing
    get: (id) => dbs?.records.get(id) as R | undefined,
    put(record) {
      const open = writable();
      if (!open) return;
      const { root, records, index } = open;
      root.transactionSync(() => {
        const previous = records.get(record.id);
        if (previous) for (const key of entries(previous)) index.removeSync(key);
        records.putSync(record.id, record);
        for (const key of entries(record)) index.putSync(key, 1);
      });
    },
    delete(id) {
      const open = writable();
      if (!open) return;
      const { root, records, index } = open;
      root.transactionSync(() => {
        const previous = records.get(id);
        if (!previous) return;
        for (const key of entries(previous)) index.removeSync(key);
        records.removeSync(id);
      });
    },
    findBy(name, value, { reverse = false, limit, cursor } = {}) {
      if (!dbs) return [];
      const { records, index } = dbs;
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
        if (limit !== undefined && found.length >= limit) break;
        if (record) found.push(record);
      }
      return found;
    },
    newId,
    close,
    reopen() {
      close();
      reopenDbs();
    },
    async reset() {
      if (readOnly) throw new Error(`The record store at ${dir} is open read-only`);
      close();
      // Let LMDB release the files before deleting them
      await new Promise((resolve) => setTimeout(resolve, 100));
      fs.rmSync(dir, { recursive: true, force: true });
      reopenDbs();
    },
    async copyTo(targetDir) {
      fs.mkdirSync(targetDir, { recursive: true });
      await current().root.backup(targetDir, false);
    },
    replaceWith(source) {
      const open = writable();
      if (!open) throw new Error(`The record store at ${dir} is closed`);
      const from = openDbs(source, true);
      const { root, records, index } = open;
      try {
        // One unit: the records the source holds, filed in this store's indexes, in place of every record here
        root.transactionSync(() => {
          // Collected first, since the body removes the keys a live cursor would walk
          for (const key of Array.from(records.getKeys())) records.removeSync(key);
          for (const key of Array.from(index.getKeys())) index.removeSync(key);
          for (const { value: record } of from.records.getRange()) {
            records.putSync(record.id, record);
            for (const key of entries(record)) index.putSync(key, 1);
          }
        });
      } finally {
        from.root.close();
      }
    },
  };
}

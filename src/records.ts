/**
 * Record stores: whole records by id, found through declared indexes in the order their ids were handed out. Not the
 * engine — a record store holds a log, such as run history, whose records are written whole and read by what they
 * are filed under, and which no unit (`transact`) ever reaches. `openRecordStore` (`@apackdev/ears/lmdb`) keeps one
 * on disk and `createMemoryRecordStore` in memory; both are the same `RecordStore`.
 */

/** A record a store holds: its id and any fields */
export type StoredRecord = { id: string } & Record<string, unknown>;

/** A value a record is filed under */
export type IndexValue = string | number | boolean;

/**
 * The indexes a store keeps, by name: each derives the value a record is filed under, or nothing (`undefined`, or
 * a value that isn't a primitive) for a record it doesn't file. An index can name a field, `(r) => r.status`, or
 * compute one, `(r) => r.parentId === undefined || undefined`, so nothing is stored only to be found by.
 */
export type RecordIndexes<R extends { id: string } = StoredRecord> = Readonly<Record<string, (record: R) => unknown>>;

/** How `findBy` walks one index value's records: oldest first unless `reverse` */
export interface FindByOptions {
  /** Newest first */
  reverse?: boolean;
  limit?: number;
  /** An id the walk resumes after, in its direction: the last id of the page before */
  cursor?: string;
}

/** A record store: its records, and the store itself — where it is, and closing, reopening, resetting and copying it */
export interface RecordStore<R extends { id: string } = StoredRecord> {
  /** Where its files are, or `':memory:'` */
  readonly path: string;
  readonly readOnly: boolean;
  /** The record with this id, or undefined */
  get(id: string): R | undefined;
  /** Writes the whole record, replacing the one with its id and refiling it in every index */
  put(record: R): void;
  /** Removes the record and its index entries; an id with no record is ignored */
  delete(id: string): void;
  /** The records filed under `value` in `index`, in id order — creation order, for ids from `newId` */
  findBy(index: string, value: IndexValue, options?: FindByOptions): R[];
  /** A new id, `<prefix>-<n>`, sorting after every id this store handed out before (a same-millisecond one too) */
  newId(prefix: string): string;
  /** Closes the store. While it is closed — a reset or an import replacing its files — reads find nothing and writes are dropped */
  close(): void;
  reopen(): void;
  /** Removes every record */
  reset(): Promise<void>;
  /** A consistent copy of the store into `targetDir`, which a store on disk opens again */
  copyTo(targetDir: string): Promise<void>;
  /** Replaces every record with those of the store at `dir`, in one unit, filing them in this store's indexes */
  replaceWith(dir: string): void;
}

const isIndexValue = (value: unknown): value is IndexValue =>
  typeof value === 'string' || typeof value === 'boolean' || (typeof value === 'number' && !Number.isNaN(value));

/** @internal Where a record is filed: each index's name and the value it files the record under */
export function _indexEntries<R extends { id: string }>(indexes: RecordIndexes<R>, record: R): Array<[string, IndexValue]> {
  return Object.entries(indexes).flatMap(([name, value]) => {
    const filed = value(record);
    return isIndexValue(filed) ? [[name, filed] as [string, IndexValue]] : [];
  });
}

/**
 * @internal A store's id clock: time-sortable ids, the clock in base 36 then a counter for ids handed out in the
 * same millisecond, so ids sort in the order they were made
 */
export function _idClock(): (prefix: string) => string {
  let lastTime = 0;
  let sameTime = 0;
  return (prefix) => {
    const now = Date.now();
    if (now > lastTime) { lastTime = now; sameTime = 0; } else sameTime++;
    return `${prefix}-${lastTime.toString(36).padStart(9, '0')}${sameTime.toString(36).padStart(4, '0')}`;
  };
}

/**
 * A record store in memory, for tests and tools: the operations and order of one on disk, and its encoding — each
 * record goes through JSON as LMDB's does, so a value the store on disk can't hold fails here too. It has no files,
 * so `copyTo` refuses and `reset` empties it.
 */
export function createMemoryRecordStore<R extends { id: string } = StoredRecord>({ indexes }: { indexes: RecordIndexes<R> }): RecordStore<R> {
  const records = new Map<string, string>();
  const decode = (json: string) => JSON.parse(json) as R;
  const newId = _idClock();
  return {
    path: ':memory:',
    readOnly: false,
    get(id) {
      const json = records.get(id);
      return json === undefined ? undefined : decode(json);
    },
    put(record) { records.set(record.id, JSON.stringify(record)); },
    delete(id) { records.delete(id); },
    findBy(index, value, { reverse = false, limit, cursor } = {}) {
      const filed = indexes[index];
      if (!filed) return [];
      let found = [...records.values()].map(decode).filter((record) => filed(record) === value)
        .sort((a, b) => (a.id < b.id ? -1 : 1));
      if (reverse) found.reverse();
      if (cursor !== undefined) found = found.filter((record) => (reverse ? record.id < cursor : record.id > cursor));
      return found.slice(0, limit);
    },
    newId,
    close() {},
    reopen() {},
    async reset() { records.clear(); },
    async copyTo() { throw new Error('A record store in memory has no files to copy'); },
    replaceWith() { throw new Error('A record store in memory has no files to replace its records with'); },
  };
}

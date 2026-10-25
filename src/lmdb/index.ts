/**
 * The database behind an app's EARS engine: an elmdb database (`openDatabase`), the one thing in `@apackdev/ears`
 * that loads elmdb, and the run history's record store on lmdb-js. Packs don't use this entry; a pack's tests reach it
 * through `@apackdev/sdk/testing`, whose test runtime opens a temporary database.
 *
 * @packageDocumentation
 */

export { openDatabase, createTemporaryDatabases, DATABASE_FORMAT, type DatabaseOptions, type TemporaryDatabases } from './database.ts';
export {
  openLmdbStore, openEnvironment, LMDB_FORMAT_VERSION, COMPACT_COPIES, SYNC_IDLE_MS, SYNC_INTERVAL_MS,
  type LmdbStore, type LmdbStoreOptions, type LmdbEnvironment,
} from './store.ts';
export { createLmdbKVBackend, type LmdbKVHooks } from './kv-backend.ts';
export { compactEnvironment, convertToCurrentFormat, convertToFormat2, type Compaction, convertToFormat3, convertToFormat4, type FormatConversion, type Format3Conversion, type Format4Conversion, type FormatSteps } from './convert.ts';
export { openRecordStore, RECORD_STORE_FORMAT_VERSION, RECORD_STORE_KIND, type RecordStoreOptions } from './record-store.ts';

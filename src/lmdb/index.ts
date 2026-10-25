/**
 * The databases behind an app's EARS engines, the app's own and its run history's: elmdb databases (`openDatabase`),
 * the one thing in `@apackdev/ears` that loads elmdb, beside the lmdb-js store and format conversions that the app's
 * data moves from. Packs don't use this entry; a pack's tests reach it through `@apackdev/sdk/testing`, whose test
 * runtime opens temporary databases.
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

/**
 * The LMDB store behind an app's EARS engine. It needs the `lmdb` package, an optional peer the app
 * installs; `@apackdev/ears` itself never loads it. Packs and pack tests don't use this entry.
 *
 * @packageDocumentation
 */

export {
  openLmdbStore, openEnvironment, LMDB_FORMAT_VERSION, COMPACT_COPIES, SYNC_IDLE_MS, SYNC_INTERVAL_MS,
  type LmdbStore, type LmdbStoreOptions, type LmdbEnvironment,
} from './store.ts';
export { createLmdbKVBackend, type LmdbKVHooks } from './kv-backend.ts';
export { compactEnvironment, convertToCurrentFormat, convertToFormat2, type Compaction, convertToFormat3, convertToFormat4, type FormatConversion, type Format3Conversion, type Format4Conversion, type FormatSteps } from './convert.ts';
export { openRecordStore, RECORD_STORE_FORMAT_VERSION, RECORD_STORE_KIND, type RecordStoreOptions } from './record-store.ts';

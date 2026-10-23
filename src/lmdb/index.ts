/**
 * The LMDB store behind an app's EARS engine. It needs the `lmdb` package, an optional peer the app
 * installs; `@apackdev/ears` itself never loads it. Packs and pack tests don't use this entry.
 *
 * @packageDocumentation
 */

export { openLmdbStore, type LmdbStore, type LmdbStoreOptions } from './store.ts';
export { LMDB_FORMAT_VERSION, openEnvAt, closeEnv, type LmdbDbs } from './envs.ts';
export { makeLmdbAdapter, type WriteFailure } from './adapter.ts';
export { hydrate } from './hydrate.ts';
export { convertToCurrentFormat, convertToFormat2, convertToFormat3, type FormatConversion, type Format3Conversion, type FormatSteps } from './convert.ts';
export { openRecordStore, RECORD_STORE_FORMAT_VERSION, RECORD_STORE_KIND, type RecordStoreOptions } from './record-store.ts';
export { LmdbReader, decodeAttr, type AttrRecord, type EntityMeta, type RelationRecord, type FindByAttrOpts } from './reader.ts';

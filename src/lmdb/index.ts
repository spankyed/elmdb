/**
 * The LMDB store behind an app's EARS engine. It needs the `lmdb` package, an optional peer the app
 * installs; `@apack/ears` itself never loads it. Packs and pack tests don't use this entry.
 *
 * @packageDocumentation
 */

export { openLmdbStore, type LmdbStore, type LmdbStoreOptions } from './store.ts';
export { LMDB_FORMAT_VERSION, openEnvAt, openShardedEnvs, closeShardedEnvs, closeEnv, deleteLmdbDirectories, type LmdbDbs, type LmdbPaths } from './envs.ts';
export { makeLmdbAdapter, type WriteFailure } from './adapter.ts';
export { hydrateSharded } from './hydrate.ts';
export { LmdbQuery, decodeAttr, type AttrRecord, type EntityMeta, type RelationRecord, type FindByAttrOpts } from './query.ts';

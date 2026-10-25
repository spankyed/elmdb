/**
 * The databases behind an app's EARS engines, the app's own and its run history's: elmdb databases (`openDatabase`),
 * the one thing in `@apackdev/ears` that loads elmdb. Packs don't use this entry; a pack's tests reach it through
 * `@apackdev/sdk/testing`, whose test runtime opens temporary databases.
 *
 * @packageDocumentation
 */

export { openDatabase, createTemporaryDatabases, DATABASE_FORMAT, type DatabaseOptions, type TemporaryDatabases } from './database.ts';

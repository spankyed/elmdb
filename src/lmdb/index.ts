/**
 * The databases behind EARS engines: elmdb databases (`openDatabase`), and temporary ones for tests and tools
 * (`createTemporaryDatabases`), the one thing in `@apackdev/ears` that loads elmdb. Code that is handed an engine never
 * needs this entry; whoever creates the engine opens its database here.
 *
 * @packageDocumentation
 */

export { openDatabase, createTemporaryDatabases, DATABASE_FORMAT, type DatabaseOptions, type TemporaryDatabases } from './database.ts';

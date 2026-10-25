import type { Schema } from './protocol/Schema.js'
import type { SchemaChange } from './protocol/SchemaChange.js'
export type { ErrorCode } from './protocol/ErrorCode.js'
export type { Schema, SchemaChange }

/**
 * Which binding artifact the generated loader actually loaded: `'native'` for
 * a native addon, otherwise the `platformArchABI` of the WASI flavor. Every
 * flavor napi-rs can build is listed, because `NAPI_RS_NATIVE_LIBRARY_PATH`
 * can point the loader at a WASI artifact this package does not build itself.
 */
export declare const __napiBindingTarget: 'native' | 'wasm32-wasi' | 'wasm32-wasip1'

/** An open database: one directory holding one LMDB environment */
export declare class Database {
  /** Opens the database at `path`, creating it unless `readOnly` */
  static open(path: string, options?: OpenOptions | undefined | null): Database
  get path(): string
  get readOnly(): boolean
  get isOpen(): boolean
  /** The schema the database holds, in its canonical form, or `null` when none was ever given */
  get schema(): Schema | null
  /** What opening did: `{ schemaChanges: [{ path, change, effect }] }` */
  get openReport(): { schemaChanges: SchemaChange[] }
  /** Group commit's syncs so far */
  get syncs(): number
  /** Resolves once every unit committed before the call is on disk, and rejects when the sync it waits on fails */
  durable(): Promise<void>
  /** Syncs what is owed and releases the environment; closing a closed database does nothing */
  close(): void
}

export interface OpenOptions {
  /** Open existing files without writing: every write is refused, and the stored schema is used */
  readOnly?: boolean
  /**
   * The schema, as the canonical JSON `docs/schema.md` describes; a schema that differs from the stored one
   * changes it
   */
  schema?: Schema
  /** Group commit's syncs, on by default; off, nothing is synced, for a test's temporary directory */
  syncs?: boolean
  syncIdleMs?: number
  syncIntervalMs?: number
  mapSizeBytes?: number
}

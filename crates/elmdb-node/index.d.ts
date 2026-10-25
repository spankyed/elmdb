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
  /**
   * Applies a batch (msgpack bytes of a `Batch`) as one unit, answering msgpack bytes of
   * `{ results: OpResult[], changes: ChangeSet }`. `durable` commits it with a sync
   */
  write(batch: Buffer, durable?: boolean | undefined | null): Buffer
  /**
   * Answers reads (msgpack bytes of `{ reads: ReadOp[] }`) from one snapshot, on this thread, as msgpack bytes of an
   * answer per read
   */
  readSync(reads: Buffer): Buffer
  /** `readSync` on a thread of the pool, in a snapshot of its own, so a large read doesn't hold up the event loop */
  read(reads: Buffer): Promise<Buffer>
  /**
   * Opens a unit that stays open across calls until it is committed or aborted; while it is, `write` refuses
   * (`UNIT_OPEN`), and writes go through the unit
   */
  begin(): Unit
  /** Resolves once every unit committed before the call is on disk, and rejects when the sync it waits on fails */
  durable(): Promise<void>
  /** Syncs what is owed and releases the environment; closing a closed database does nothing */
  close(): void
}

/** A unit open across calls, on the thread that began it */
export declare class Unit {
  /**
   * Applies a batch (msgpack bytes) in the unit, keeping none of it when any operation is refused; answers msgpack
   * bytes of `{ results: OpResult[] }`
   */
  write(batch: Buffer): Buffer
  /** Answers reads from the unit's transaction, which sees the unit's own writes */
  read(reads: Buffer): Buffer
  /** Opens a savepoint, answering its depth */
  savepoint(): number
  /** Undoes everything since the savepoint at `depth`, closing it */
  rollbackTo(depth: number): void
  /** Closes the savepoint at `depth`, keeping its writes */
  release(depth: number): void
  /** Commits the unit after its deferred checks, answering msgpack bytes of its `ChangeSet`; a failed check aborts it */
  commit(durable?: boolean | undefined | null): Buffer
  /** Ends the unit, keeping nothing of it */
  abort(): void
  get isOpen(): boolean
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

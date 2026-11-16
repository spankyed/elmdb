// The thin client's declarations: the binding's reads and writes, typed with the protocol's generated types
// (`protocol/`), over the caller's msgpack codec (`client.js`). Only the calls are written here; every type they take
// or answer is generated from the Rust.
import type { Database, Unit, Reads, Answers, Written, BatchResult, ChangeSet } from './index.js'
import type { Batch } from './protocol/Batch.js'

/**
 * The caller's msgpack: values encoded and decoded as its program wants them (times, extensions). `unpack` decodes
 * each map as an object, and `unpackMaps` each as a `Map`, for an answer whose maps may hold a key `__proto__`. Both
 * write and read plain msgpack: with msgpackr, two `Packr`s with `useRecords: false`, as elmdb refuses its record
 * extension, that differ only in `mapsAsObjects`
 */
export interface Codec {
  pack(value: unknown): Buffer
  unpack(bytes: Buffer): unknown
  unpackMaps(bytes: Buffer): unknown
}

/**
 * A database's reads and writes as the protocol's types. An answer whose `plainObjects` is false is decoded with maps
 * as `Map`s, and each map made an object with no prototype, so a key `__proto__` is kept as a key. The binding's other
 * calls are on `db`
 */
export declare class Client {
  constructor(db: Database, codec: Codec)
  readonly db: Database
  readonly codec: Codec
  /** `Database.read`: an answer per read, in the order asked */
  read(reads: Reads): Answers
  /** `Database.readAsync`, on a thread of the pool */
  readAsync(reads: Reads): Promise<Answers>
  /** `Database.write`: the batch applied and committed, `durable` with a sync */
  write(batch: Batch, durable?: boolean): Written
  /** `Database.begin`: a unit, which every write goes through until it commits or aborts */
  begin(): ClientUnit
}

/** A unit's reads and writes as the protocol's types; its savepoints and checkpoints are on `unit` */
export declare class ClientUnit {
  constructor(unit: Unit, codec: Codec)
  readonly unit: Unit
  readonly codec: Codec
  /** `Unit.read`, which sees the unit's own writes */
  read(reads: Reads): Answers
  /** `Unit.write`: the batch applied in the unit, or none of it */
  write(batch: Batch): BatchResult
  /** `Unit.pendingChanges`: what the unit has changed so far */
  pendingChanges(): ChangeSet
  /** `Unit.commit`, after its deferred checks; a failed check aborts it */
  commit(durable?: boolean): ChangeSet
  /** `Unit.abort`: the unit ends, keeping nothing */
  abort(): void
}

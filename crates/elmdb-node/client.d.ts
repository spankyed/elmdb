// Clean-room elmdb v2 zero-dependency binary wire batch encoder declarations (`elmdb-wire` EWB2).

/** Branded intra-batch entity reference returned by `WireBatchBuilder.create(...)`. */
export declare class BatchSlot {
  readonly slot: number
  constructor(index: number)
}

export type EntityTarget = string | BatchSlot

export interface LinkOptions {
  weight?: number
  info?: unknown
}

/**
 * Fluent builder encoding atomic `elmdb-wire` (`EWB2`) binary batch frames for `Database.executeWireBatch`.
 */
export declare class WireBatchBuilder {
  constructor()
  create(typeName: string, fields: Record<string, unknown>, externalId?: string): BatchSlot
  patch(target: EntityTarget, fields: Record<string, unknown>): this
  unset(target: EntityTarget, fields: string[]): this
  trash(target: EntityTarget): this
  restore(target: EntityTarget): this
  destroy(target: EntityTarget): this
  link(source: EntityTarget, relKind: string, target: EntityTarget, opts?: LinkOptions): this
  unlink(source: EntityTarget, relKind?: string, target?: EntityTarget): this
  readonly length: number
  encode(returnFlags?: number): Uint8Array
}

/**
 * Synchronously builds and encodes an `elmdb-wire` binary batch frame.
 * Throws `TypeError` if `fn` returns a Promise/thenable.
 */
export declare function buildWireBatch(fn: (batch: WireBatchBuilder) => void): Uint8Array

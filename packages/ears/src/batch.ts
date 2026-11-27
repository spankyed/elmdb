import { BatchSlot, WireBatchBuilder, type EntityTarget, type LinkOptions } from 'elmdb/client';
import { EarsError, toEarsError } from './errors.ts';

export { BatchSlot };
export type { EntityTarget, LinkOptions };

/**
 * Fluent builder for constructing a single atomic `elmdb-wire` (`EWB2`) binary write batch.
 *
 * Every mutation inside a batch executes in a single LMDB write transaction and a single FFI crossing.
 * Entities created in the batch return a `BatchSlot` that subsequent `patch`/`link`/`unlink` operations
 * in the same batch can target without waiting for a commit.
 */
export interface BatchWriter {
  create(typeName: string, fields: Record<string, unknown>, externalId?: string): BatchSlot;
  patch(target: EntityTarget, fields: Record<string, unknown>): this;
  unset(target: EntityTarget, fields: readonly string[]): this;
  trash(target: EntityTarget): this;
  restore(target: EntityTarget): this;
  destroy(target: EntityTarget): this;
  link(source: EntityTarget, relKind: string, target: EntityTarget, opts?: LinkOptions): this;
  unlink(source: EntityTarget, relKind?: string, target?: EntityTarget): this;
  readonly length: number;
  encode(returnFlags?: number): Uint8Array;
}

class WireBatchWriterImpl implements BatchWriter {
  readonly #inner = new WireBatchBuilder();

  create(typeName: string, fields: Record<string, unknown>, externalId?: string): BatchSlot {
    return externalId === undefined
      ? this.#inner.create(typeName, fields)
      : this.#inner.create(typeName, fields, externalId);
  }

  patch(target: EntityTarget, fields: Record<string, unknown>): this {
    this.#inner.patch(target, fields);
    return this;
  }

  unset(target: EntityTarget, fields: readonly string[]): this {
    this.#inner.unset(target, [...fields]);
    return this;
  }

  trash(target: EntityTarget): this {
    this.#inner.trash(target);
    return this;
  }

  restore(target: EntityTarget): this {
    this.#inner.restore(target);
    return this;
  }

  destroy(target: EntityTarget): this {
    this.#inner.destroy(target);
    return this;
  }

  link(source: EntityTarget, relKind: string, target: EntityTarget, opts?: LinkOptions): this {
    if (opts === undefined) this.#inner.link(source, relKind, target);
    else this.#inner.link(source, relKind, target, opts);
    return this;
  }

  unlink(source: EntityTarget, relKind?: string, target?: EntityTarget): this {
    if (relKind === undefined && target === undefined) this.#inner.unlink(source);
    else if (target === undefined) this.#inner.unlink(source, relKind);
    else this.#inner.unlink(source, relKind, target);
    return this;
  }

  get length(): number {
    return this.#inner.length;
  }

  encode(returnFlags?: number): Uint8Array {
    try {
      return returnFlags === undefined ? this.#inner.encode() : this.#inner.encode(returnFlags);
    } catch (error) {
      throw toEarsError(error, 'INVALID_ARGUMENT');
    }
  }
}

/** Creates a standalone `BatchWriter` backed by the canonical `elmdb-wire` binary encoder. */
export function createBatchWriter(): BatchWriter {
  return new WireBatchWriterImpl();
}

/** Internal helper detecting Promise/thenable return values from synchronous-only callbacks. */
export function isThenable(value: unknown): boolean {
  if (value === null || (typeof value !== 'object' && typeof value !== 'function')) return false;
  return typeof (value as { readonly then?: unknown }).then === 'function';
}

/**
 * Invokes a synchronous callback and rejects Promises/thenables inside the same `try/catch`
 * so hostile throwing `.then` getters are wrapped cleanly as `EarsError('INVALID_ARGUMENT')`
 * while preserving `EarsError('ASYNC_CALLBACK_FORBIDDEN')`.
 */
export function runSyncCallback<TArg, TResult>(
  fn: (arg: TArg) => TResult,
  arg: TArg,
  asyncErrorMessage: string,
): TResult {
  try {
    const returned = fn(arg);
    if (isThenable(returned)) {
      throw new EarsError('ASYNC_CALLBACK_FORBIDDEN', asyncErrorMessage);
    }
    return returned;
  } catch (error) {
    if (error instanceof EarsError && error.code === 'ASYNC_CALLBACK_FORBIDDEN') {
      throw error;
    }
    throw toEarsError(error, 'INVALID_ARGUMENT');
  }
}

/**
 * Synchronously populates and encodes an atomic `elmdb-wire` binary batch.
 * Rejects Promises/thenables with `EarsError('ASYNC_CALLBACK_FORBIDDEN')`.
 */
export function encodeBatch(fn: (batch: BatchWriter) => void, returnFlags?: number): Uint8Array {
  const writer = createBatchWriter();
  runSyncCallback(
    fn,
    writer,
    'Batch callbacks must be synchronous (Promises and async callbacks are forbidden)',
  );
  return writer.encode(returnFlags);
}

import { EarsError } from './errors.ts';
import type { WatchReadContext } from './watch.ts';

/** The operation crossing the engine boundary; observer work never changes its outcome. */
export interface EarsOperation {
  kind: 'read' | 'write' | 'unit' | 'load';
  ops: number;
  watch: boolean;
  inUnit: boolean;
  /** The handle's reported sequence, not a read snapshot ID; absent when unavailable. */
  sequence?: number;
}

/** Result metadata only: no query parameters, values or error messages enter observations. */
export interface EarsOperationResult {
  outcome: 'ok' | 'error' | 'committed-not-durable';
  /** The handle's reported sequence, not a read snapshot ID; absent when unavailable. */
  sequence?: number;
  errorCode?: string;
  requestBytes: number;
  responseBytes: number;
}

/** Optional instrumentation owned by the engine's caller; the caller chooses context and recording policy. */
export interface EarsOperationObserver {
  start(operation: Readonly<EarsOperation>): (result: Readonly<EarsOperationResult>) => void;
}

/** Connection-local accounting; diagnostic callbacks cannot alter or recursively observe application work. */
export function createOperationObservation(state: () => Pick<EarsOperation, 'watch' | 'inUnit'>, sequence: () => number) {
  const observers = new Set<EarsOperationObserver>();
  let notifying = false;
  const frames: Array<{ requestBytes: number; responseBytes: number }> = [];
  const observedSequence = (): number | undefined => {
    try { return sequence(); } catch { return undefined; }
  };
  function run<T>(kind: EarsOperation['kind'], ops: number, fn: () => T): T {
    if (observers.size === 0 || notifying) return fn();
    const operation: EarsOperation = { kind, ops, ...state(), sequence: observedSequence() };
    const finishes: Array<ReturnType<EarsOperationObserver['start']>> = [];
    // Registration changes inside a callback affect the next operation, not this delivery.
    const participants = Array.from(observers);
    notifying = true;
    try {
      for (const observer of participants) {
        try { finishes.push(observer.start(Object.freeze({ ...operation }))); } catch { /* Diagnostics cannot refuse an operation. */ }
      }
    } finally { notifying = false; }
    const frame = { requestBytes: 0, responseBytes: 0 };
    frames.push(frame);
    let outcome: EarsOperationResult['outcome'] = 'ok';
    let errorCode: string | undefined;
    try { return fn(); }
    catch (error) {
      // Codes are engine-owned; never record a user-supplied error message or payload.
      try {
        errorCode = error instanceof EarsError ? error.code
          : (error as { code?: unknown } | null)?.code === 'NOT_DURABLE' ? 'NOT_DURABLE' : undefined;
      } catch { /* Error metadata may have throwing getters; preserve the original error. */ }
      const sequence = observedSequence();
      outcome = errorCode === 'NOT_DURABLE' && sequence !== undefined && operation.sequence !== undefined && sequence > operation.sequence ? 'committed-not-durable' : 'error';
      throw error;
    } finally {
      frames.pop();
      const result = Object.freeze({ ...frame, outcome, errorCode, sequence: observedSequence() });
      notifying = true;
      try {
        for (const finish of finishes) {
          try { finish(result); } catch { /* Preserve the original return value or error. */ }
        }
      } finally { notifying = false; }
    }
  }
  function request(bytes: Uint8Array): Uint8Array {
    const frame = notifying ? undefined : frames.at(-1);
    if (frame) frame.requestBytes += bytes.byteLength;
    return bytes;
  }
  function response(bytes: Uint8Array): Uint8Array {
    const frame = notifying ? undefined : frames.at(-1);
    if (frame) frame.responseBytes += bytes.byteLength;
    return bytes;
  }

  return {
    get active() { return observers.size > 0 && !notifying; },
    run, request, response,
    observe(observer: EarsOperationObserver) {
      observers.add(observer);
      return () => { observers.delete(observer); };
    },
  };
}

/** Bounded commit causes belong to observation, not the watch scheduler's invalidation policy. */
export class WatchCauses {
  #pending: { sequences: Set<number>; dropped: number } | undefined;
  #record: () => boolean;
  constructor(record: () => boolean) { this.#record = record; }
  add(sequence: number) {
    try {
      if (!this.#record()) { this.#pending = undefined; return; }
      const causes = this.#pending ??= { sequences: new Set(), dropped: 0 };
      if (causes.sequences.size < 32 || causes.sequences.has(sequence)) causes.sequences.add(sequence);
      else causes.dropped++;
    } catch {
      const causes = this.#pending ??= { sequences: new Set(), dropped: 0 };
      causes.dropped++;
    }
  }
  take(): WatchReadContext | undefined {
    const causes = this.#pending;
    this.#pending = undefined;
    return causes && Object.freeze({ sequences: Object.freeze([...causes.sequences]), dropped: causes.dropped });
  }
}

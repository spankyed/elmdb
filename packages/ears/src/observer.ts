import type { EarsErrorCode } from './errors.ts';

/** Category of an observed `@elmdb/ears` engine operation. */
export type EarsOperationKind = 'read' | 'write' | 'unit' | 'load';

/** Metadata describing an `@elmdb/ears` operation when it starts. */
export interface EarsOperation {
  readonly kind: EarsOperationKind;
  readonly ops: number;
  readonly watch: boolean;
  readonly inUnit: boolean;
  readonly sequence?: number;
}

/** Completion outcome passed to the finisher returned by `EarsOperationObserver.start`. */
export interface EarsOperationResult {
  readonly outcome: 'ok' | 'error' | 'aborted';
  readonly sequence?: number;
  readonly requestBytes: number;
  readonly responseBytes: number;
  readonly errorCode?: EarsErrorCode;
}

/**
 * Opt-in synchronous operation observer for telemetry (e.g. OpenTelemetry `ears.*` spans)
 * or test operation counting. Incurs zero overhead when omitted.
 */
export interface EarsOperationObserver {
  start(operation: EarsOperation): (result: EarsOperationResult) => void;
}

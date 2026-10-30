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

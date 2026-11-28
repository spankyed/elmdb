import type { ErrorCode } from 'elmdb';

/**
 * Canonical error codes produced by elmdb v2 and `@elmdb/ears`.
 * Single source of truth generated from `crates/elmdb-wire/src/error.rs`.
 */
export type EarsErrorCode = ErrorCode;

/** Field names reserved by the elmdb v2 wire protocol and row model. */
export const RESERVED_FIELDS: readonly string[] = Object.freeze([
  '__proto__',
  'id',
  'typeName',
  'trashed',
]);

/** Entity type names reserved by the engine. */
export const RESERVED_ENTITY_TYPES: readonly string[] = Object.freeze([
  '__proto__',
]);

const KNOWN_ERROR_CODES: Readonly<Record<ErrorCode, true>> = Object.freeze({
  INVALID_ARGUMENT: true,
  INVALID_WIRE_FRAME: true,
  INVALID_SCHEMA: true,
  SCHEMA_CHANGED: true,
  UNKNOWN_TYPE: true,
  TYPE_ARCHIVED: true,
  UNKNOWN_FIELD: true,
  UNKNOWN_RELATION: true,
  UNKNOWN_INDEX: true,
  ENTITY_NOT_FOUND: true,
  ENTITY_ALREADY_EXISTS: true,
  ENTITY_TRASHED: true,
  ENTITY_NOT_TRASHED: true,
  RELATION_NOT_FOUND: true,
  RELATION_ALREADY_EXISTS: true,
  VALIDATION_FAILED: true,
  UNIQUE_VIOLATION: true,
  CARDINALITY_VIOLATION: true,
  CYCLE_DETECTED: true,
  REFERENCED: true,
  ENDPOINT_MISMATCH: true,
  HOOK_ABORTED: true,
  HOOK_CYCLE_LIMIT: true,
  ASYNC_CALLBACK_FORBIDDEN: true,
  INDEX_BUILD_FAILED: true,
  INDEX_NOT_READY: true,
  RESOURCE_LIMIT: true,
  CANCELLED: true,
  STORAGE_FULL: true,
  CORRUPTED: true,
  DURABILITY_FAILED: true,
  POISONED: true,
  IO: true,
  INTERNAL: true,
  CLOSED: true,
  READ_ONLY: true,
  NO_DATABASE: true,
  DATABASE_LOCKED: true,
  ALREADY_EXISTS: true,
  PANICKED: true,
  ALREADY_LOADED: true,
});

const NAPI_INVALID_ARG_CODES: Readonly<Record<string, true>> = Object.freeze({
  InvalidArg: true,
  ObjectExpected: true,
  StringExpected: true,
  NameExpected: true,
  FunctionExpected: true,
  NumberExpected: true,
  BooleanExpected: true,
  ArrayExpected: true,
  DateExpected: true,
  BigIntExpected: true,
});

/**
 * Typed error raised by `@elmdb/ears` carrying a canonical `EarsErrorCode`.
 */
export class EarsError extends Error {
  readonly code: EarsErrorCode;

  constructor(code: EarsErrorCode, message: string, options?: { readonly cause?: unknown }) {
    super(message, options?.cause !== undefined ? { cause: options.cause } : undefined);
    this.name = 'EarsError';
    this.code = code;
  }
}

/** Type guard for `EarsError`. */
export function isEarsError(error: unknown): error is EarsError {
  return error instanceof EarsError;
}

function safeReadProperty(obj: object, prop: string): unknown {
  try {
    return (obj as Record<string, unknown>)[prop];
  } catch {
    return undefined;
  }
}

function safeStringify(value: unknown): string {
  if (typeof value === 'string') return value;
  if (typeof value === 'symbol') return value.toString();
  try {
    return String(value);
  } catch {
    return '[unstringifiable error]';
  }
}

/**
 * Returns true if `error` is an `EarsError` or carries a canonical elmdb / N-API error code.
 * Used by `transact()` to wrap engine errors into `EarsError` while preserving user domain errors verbatim.
 */
export function hasKnownErrorCode(error: unknown): boolean {
  if (error instanceof EarsError) return true;
  if (typeof error === 'object' && error !== null) {
    const maybeCode = safeReadProperty(error, 'code');
    if (typeof maybeCode === 'string') {
      return (
        Object.prototype.hasOwnProperty.call(KNOWN_ERROR_CODES, maybeCode)
        || Object.prototype.hasOwnProperty.call(NAPI_INVALID_ARG_CODES, maybeCode)
      );
    }
  }
  return false;
}

/**
 * Normalizes any thrown error (native `elmdb` N-API error or JS error) into an `EarsError`
 * while preserving its canonical `code` and original error as `cause`.
 */
export function toEarsError(error: unknown, fallbackCode: EarsErrorCode = 'INTERNAL'): EarsError {
  if (error instanceof EarsError) return error;
  if (typeof error === 'object' && error !== null) {
    const maybeCode = safeReadProperty(error, 'code');
    const maybeMessage = safeReadProperty(error, 'message');
    let code: EarsErrorCode = fallbackCode;
    if (typeof maybeCode === 'string') {
      if (Object.prototype.hasOwnProperty.call(KNOWN_ERROR_CODES, maybeCode)) {
        code = maybeCode as EarsErrorCode;
      } else if (Object.prototype.hasOwnProperty.call(NAPI_INVALID_ARG_CODES, maybeCode)) {
        code = 'INVALID_ARGUMENT';
      }
    }
    const message = typeof maybeMessage === 'string' && maybeMessage.length > 0 ? maybeMessage : safeStringify(error);
    return new EarsError(code, message, { cause: error });
  }
  return new EarsError(fallbackCode, safeStringify(error), { cause: error });
}

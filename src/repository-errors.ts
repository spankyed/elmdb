export const RepositoryErrorCode = {
  NOT_FOUND: 'NOT_FOUND',
  VALIDATION_ERROR: 'VALIDATION_ERROR',
  CONSTRAINT_VIOLATION: 'CONSTRAINT_VIOLATION',
  PERMISSION_DENIED: 'PERMISSION_DENIED',
  CONCURRENCY_ERROR: 'CONCURRENCY_ERROR',
  OPERATION_FAILED: 'OPERATION_FAILED',
  UNKNOWN: 'UNKNOWN',
} as const;

export type RepositoryErrorCode = typeof RepositoryErrorCode[keyof typeof RepositoryErrorCode];

export class RepositoryError extends Error {
  constructor(
    message: string,
    public code: RepositoryErrorCode = RepositoryErrorCode.UNKNOWN,
    public details?: unknown
  ) {
    super(message);
    this.name = 'RepositoryError';
  }
}

import { ApiError } from "../api";

export function retryableReadError(error: unknown): boolean {
  return (
    error instanceof TypeError ||
    (error instanceof ApiError &&
      ((error.status >= 500 && error.status < 600 && error.status !== 501) ||
        error.status === 408 ||
        error.status === 429))
  );
}

export function retryableTranscriptPullConflict(
  error: unknown,
  scope: string,
): boolean {
  // The transcript pull route maps only SnapshotChanged to HTTP 409; keep that
  // optimistic-read retry scoped so conflicts on other reads stay permanent.
  return (
    scope.startsWith("transcript:") &&
    error instanceof ApiError &&
    error.status === 409
  );
}

export function readRetryDelay(attempt: number): number {
  return Math.min(1_000 * 2 ** Math.min(attempt, 5), 30_000);
}

/** Maximum failed read attempts per recovery signal, including the initial attempt. */
export const READ_RECOVERY_MAX_ATTEMPTS = 6;

export class ReadRecoveryExhaustedError extends Error {
  constructor(cause: unknown) {
    super(
      "Read recovery exhausted. Retry or wait for a new change or reconnection.",
      {
        cause,
      },
    );
    this.name = "ReadRecoveryExhaustedError";
  }
}

/** Shared by HTTP projection pulls and notified resource reads. */
export class ReadRecoveryBudget {
  private failures = 0;
  exhaustedError: ReadRecoveryExhaustedError | undefined;

  nextDelay(error: unknown, backoffAttempt = this.failures): number {
    if (this.exhaustedError) throw this.exhaustedError;
    if (++this.failures >= READ_RECOVERY_MAX_ATTEMPTS) {
      this.exhaustedError = new ReadRecoveryExhaustedError(error);
      throw this.exhaustedError;
    }
    return readRetryDelay(backoffAttempt);
  }
}

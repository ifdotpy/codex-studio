import { ApiError } from "../api";

export function retryableReadError(error: unknown): boolean {
  return (
    error instanceof TypeError ||
    (error instanceof ApiError &&
      (error.status >= 500 || error.status === 408 || error.status === 429))
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

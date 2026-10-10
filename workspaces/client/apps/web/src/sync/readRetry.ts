import { ApiError } from "../api";

export function retryableReadError(error: unknown): boolean {
  return (
    error instanceof TypeError ||
    (error instanceof ApiError &&
      (error.status >= 500 || error.status === 408 || error.status === 429))
  );
}

export function readRetryDelay(attempt: number): number {
  return Math.min(1_000 * 2 ** Math.min(attempt, 5), 30_000);
}

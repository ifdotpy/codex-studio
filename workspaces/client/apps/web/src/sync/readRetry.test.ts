import { describe, expect, it } from "vitest";
import { ApiError, NetworkTimeoutError } from "../api";
import {
  READ_RECOVERY_MAX_ATTEMPTS,
  ReadRecoveryBudget,
  ReadRecoveryExhaustedError,
  readRetryDelay,
  retryableReadError,
  retryableTranscriptPullConflict,
} from "./readRetry";

describe("failed resource reads", () => {
  it.each([
    new TypeError("Failed to fetch"),
    new NetworkTimeoutError(),
    new ApiError("Timeout", 408),
    new ApiError("Busy", 429),
    new ApiError("Internal", 500),
    new ApiError("Bad gateway", 502),
    new ApiError("Unavailable", 503),
    new ApiError("Gateway timeout", 504),
    new ApiError("HTTP version not supported", 505),
  ])("retries a transient read error: %s", (error) => {
    expect(retryableReadError(error)).toBe(true);
  });

  it.each([
    new ApiError("Permission", 401),
    new ApiError("Permission", 403),
    new ApiError("Missing", 404),
    new ApiError("Conflict", 409),
    new ApiError("Not implemented", 501),
    new ApiError("Pending", 400),
    new Error("Workspace changed"),
    new DOMException("Aborted", "AbortError"),
  ])("holds a permanent or cancelled read: %s", (error) => {
    expect(retryableReadError(error)).toBe(false);
  });

  it("bounds repeated failures to one attempt per thirty seconds", () => {
    expect(
      Array.from({ length: 8 }, (_, attempt) => readRetryDelay(attempt)),
    ).toEqual([1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000, 30_000]);
  });

  it("reports exhaustion with its HTTP cause and cannot itself be retried", () => {
    const error = new ApiError("Unavailable", 503);
    const budget = new ReadRecoveryBudget();
    const delays = [1_000, 2_000, 4_000, 8_000, 16_000];
    expect(delays).toHaveLength(READ_RECOVERY_MAX_ATTEMPTS - 1);
    for (const delay of delays) expect(budget.nextDelay(error)).toBe(delay);
    expect(() => budget.nextDelay(error)).toThrow(ReadRecoveryExhaustedError);
    expect(budget.exhaustedError?.cause).toBe(error);
    expect(retryableReadError(budget.exhaustedError)).toBe(false);
    expect(() => budget.nextDelay(error)).toThrow(budget.exhaustedError);
  });

  it("retries 409 only for transcript pulls, not other reads", () => {
    const conflict = new ApiError("Conflict", 409);
    expect(retryableTranscriptPullConflict(conflict, "transcript:agent")).toBe(
      true,
    );
    expect(retryableReadError(conflict)).toBe(false);
    expect(retryableTranscriptPullConflict(conflict, "state")).toBe(false);
    expect(
      retryableTranscriptPullConflict(
        new ApiError("Unavailable", 503),
        "transcript:agent",
      ),
    ).toBe(false);
  });
});

import { describe, expect, it } from "vitest";
import { ApiError, NetworkTimeoutError } from "../api";
import { readRetryDelay, retryableReadError } from "./readRetry";

describe("failed resource reads", () => {
  it.each([
    new TypeError("Failed to fetch"),
    new NetworkTimeoutError(),
    new ApiError("Timeout", 408),
    new ApiError("Busy", 429),
    new ApiError("Unavailable", 503),
  ])("retries a transient read error: %s", (error) => {
    expect(retryableReadError(error)).toBe(true);
  });

  it.each([
    new ApiError("Permission", 401),
    new ApiError("Permission", 403),
    new ApiError("Missing", 404),
    new ApiError("Conflict", 409),
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
});

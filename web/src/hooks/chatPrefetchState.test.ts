import { describe, expect, it } from "vitest";
import { foregroundTranscriptPending } from "./chatPrefetchState";

describe("chat prefetch foreground gate", () => {
  it("allows explicit prefetches when no managed chat is foreground", () => {
    expect(foregroundTranscriptPending(null, false)).toBe(false);
  });

  it("waits for the selected managed transcript cache", () => {
    expect(foregroundTranscriptPending("managed-agent", false)).toBe(true);
    expect(foregroundTranscriptPending("managed-agent", true)).toBe(false);
  });
});

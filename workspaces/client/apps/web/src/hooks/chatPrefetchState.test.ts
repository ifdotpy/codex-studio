import { describe, expect, it } from "vitest";
import {
  foregroundTranscriptPending,
  TranscriptRefreshGate,
} from "./chatPrefetchState";

describe("chat prefetch foreground gate", () => {
  it("allows explicit prefetches when no managed chat is foreground", () => {
    expect(foregroundTranscriptPending(null, false)).toBe(false);
  });

  it("waits for the selected managed transcript cache", () => {
    expect(foregroundTranscriptPending("managed-agent", false)).toBe(true);
    expect(foregroundTranscriptPending("managed-agent", true)).toBe(false);
  });
});

describe("chat prefetch transcript refresh gate", () => {
  const current = { epoch: "epoch-one", revision: 42 };

  it("suppresses the same revision only until the history pull settles", () => {
    const gate = new TranscriptRefreshGate();
    gate.start("agent-b", current);

    expect(gate.shouldSuppress("agent-b", current)).toBe(true);
    gate.historySettled("agent-b");

    // A later notification during the combined progress read is actionable.
    expect(gate.shouldSuppress("agent-b", current)).toBe(false);
    expect(gate.finish("agent-b", true, false)).toBe(false);
  });

  it("allows one retry when an echoed recovery revision is followed by failure", () => {
    const gate = new TranscriptRefreshGate();
    gate.start("agent-b", current);

    expect(gate.shouldSuppress("agent-b", current)).toBe(true);
    expect(gate.finish("agent-b", false, false)).toBe(true);
    expect(gate.finish("agent-b", false, false)).toBe(false);
  });

  it("does not suppress a newer revision during the active pull", () => {
    const gate = new TranscriptRefreshGate();
    gate.start("agent-b", current);

    expect(gate.shouldSuppress("agent-b", { ...current, revision: 43 })).toBe(
      false,
    );
    expect(gate.finish("agent-b", false, false)).toBe(false);
  });

  it("forgets recovery markers when a transcript watcher is removed", () => {
    const gate = new TranscriptRefreshGate();
    gate.start("agent-b", current);
    expect(gate.shouldSuppress("agent-b", current)).toBe(true);

    gate.forget("agent-b");
    expect(gate.finish("agent-b", false, false)).toBe(false);
    gate.start("agent-b", current);

    expect(gate.shouldSuppress("agent-b", current)).toBe(true);
    expect(gate.finish("agent-b", false, false)).toBe(true);
  });
});

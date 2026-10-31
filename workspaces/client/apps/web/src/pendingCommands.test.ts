import { describe, expect, it, vi } from "vitest";
import {
  PENDING_COMMAND_MAX_MS,
  createPendingCommands,
} from "./pendingCommands";

describe("pending layer", () => {
  it("shows the expected result at once and clears when the client has the row", () => {
    const pending = createPendingCommands();
    const seen = vi.fn();
    pending.subscribe(seen);
    pending.begin({ id: "c1", kind: "stop", target: "chat" });
    expect(pending.list().map((entry) => entry.id)).toEqual(["c1"]);
    pending.settle("c1", 10);
    expect(pending.list()).toHaveLength(1);
    pending.observeClientSeq(9);
    expect(pending.list()).toHaveLength(1);
    pending.observeClientSeq(10);
    expect(pending.list()).toHaveLength(0);
    expect(seen).toHaveBeenCalled();
  });

  it("clears at once when the client already holds the response sequence", () => {
    const pending = createPendingCommands();
    pending.observeClientSeq(20);
    pending.begin({ id: "c1", kind: "withdraw", target: "chat", message: "m" });
    pending.settle("c1", 15);
    expect(pending.list()).toHaveLength(0);
  });

  it("keeps the entry until the stream catches up when it arrives before the response", () => {
    const pending = createPendingCommands();
    pending.begin({ id: "c1", kind: "stop", target: "chat" });
    pending.observeClientSeq(50);
    expect(pending.list()).toHaveLength(1);
    pending.settle("c1", 40);
    expect(pending.list()).toHaveLength(0);
  });

  it("removes a failed command", () => {
    const pending = createPendingCommands();
    pending.begin({ id: "c1", kind: "send", target: "chat" });
    pending.fail("c1");
    expect(pending.list()).toHaveLength(0);
  });

  it("removes an entry that the stream never confirms", () => {
    vi.useFakeTimers();
    try {
      const pending = createPendingCommands();
      pending.begin({ id: "c1", kind: "stop", target: "chat" });
      pending.settle("c1", 99);
      vi.advanceTimersByTime(PENDING_COMMAND_MAX_MS);
      expect(pending.list()).toHaveLength(0);
    } finally {
      vi.useRealTimers();
    }
  });
});

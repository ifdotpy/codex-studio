import { describe, expect, it } from "vitest";
import type { Message } from "../../../types";
import { activitySummary } from "./Activity";

function toolMessage(value: unknown): Message {
  return {
    id: "tool-item",
    role: "tool",
    text: JSON.stringify(value),
  };
}

describe("activitySummary", () => {
  it("keeps malformed mixed command actions from being read-only", () => {
    const summary = activitySummary([
      toolMessage({
        type: "commandExecution",
        commandActions: [{ type: "read", path: "README.md" }, null],
      }),
    ]);

    expect(summary.label).toContain("Read 1 file");
    expect(summary.label).toContain("Ran 1 command");
  });

  it("treats a present nonzero string exit code as failed", () => {
    const summary = activitySummary([
      toolMessage({ type: "commandExecution", exitCode: "1" }),
    ]);

    expect(summary.failed).toBe(1);
  });
});

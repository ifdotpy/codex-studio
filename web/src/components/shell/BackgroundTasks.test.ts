import { describe, expect, it } from "vitest";
import { monitorTask } from "./BackgroundTasks";

describe("monitorTask", () => {
  it("adds the task kind while preserving its typed monitor details", () => {
    expect(
      monitorTask({
        id: "monitor-1",
        agent: "agent-1",
        created: 123,
        status: "running",
        command: "npm run check",
        cancelRequested: false,
      }),
    ).toEqual({
      id: "monitor-1",
      agent: "agent-1",
      created: 123,
      kind: "monitor",
      status: "running",
      command: "npm run check",
      cancelRequested: false,
    });
  });

  it("drops statuses without a background task meaning", () => {
    expect(
      monitorTask({
        id: "monitor-1",
        agent: "agent-1",
        created: 123,
        status: "unknown",
      }),
    ).toBeNull();
  });
});

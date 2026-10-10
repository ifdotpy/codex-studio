import { describe, expect, it } from "vitest";
import { monitorTask } from "./BackgroundTasks";

describe("monitorTask", () => {
  it("narrows a complete API monitor while preserving its task details", () => {
    expect(
      monitorTask({
        id: "monitor-1",
        agent: "agent-1",
        created: 123,
        status: "running",
        command: "npm run check",
        cancelRequested: false,
      }),
    ).toMatchObject({
      id: "monitor-1",
      agent: "agent-1",
      created: 123,
      kind: "monitor",
      status: "running",
      command: "npm run check",
      cancelRequested: false,
    });
  });

  it("omits monitors whose nullable identity fields cannot form a task row", () => {
    expect(
      monitorTask({
        id: "monitor-2",
        agent: null,
        created: 123,
        status: "running",
      }),
    ).toBeNull();
    expect(
      monitorTask({
        id: "monitor-3",
        agent: "agent-1",
        created: null,
        status: "running",
      }),
    ).toBeNull();
  });
});

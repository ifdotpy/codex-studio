import { describe, expect, it } from "vitest";
import {
  backgroundProcessGroups,
  monitorTask,
  responseOwnerIdentityKey,
  responseStopIdentity,
} from "./BackgroundTasks";

describe("background process groups", () => {
  it("renders Claude background tasks in the task list", () => {
    expect(backgroundProcessGroups).toContainEqual([
      "backgroundTask",
      "Background tasks",
    ]);
  });
});

describe("response stop identity", () => {
  it("changes when the owner moves from idle to active or starts a new turn", () => {
    const idle = {
      id: "lead",
      accountKey: "default",
      epoch: 4,
      inFlight: false,
      threadId: "thread-current",
      turnId: null,
    };
    const active = { ...idle, inFlight: true, turnId: "turn-1" };
    const nextTurn = { ...active, turnId: "turn-2" };
    expect(responseOwnerIdentityKey(idle)).not.toBe(
      responseOwnerIdentityKey(active),
    );
    expect(responseOwnerIdentityKey(active)).not.toBe(
      responseOwnerIdentityKey(nextTurn),
    );
  });

  it("sends the exact server-listed connection, thread, and turn", () => {
    const target = {
      agent: "lead",
      accountKey: "default",
      epoch: 4,
      connectionId: "connection-current",
      threadId: "thread-current",
      turnId: "turn-current",
    };
    expect(responseStopIdentity(target)).toEqual({ responseTarget: target });
    expect(responseStopIdentity(null)).toEqual({});
  });
});

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

describe("dispatch strip states", () => {
  it("prioritizes approvals and keeps the existing command projection", async () => {
    const { backgroundTaskState } = await import("./BackgroundTasks");
    const task = {
      id: "monitor",
      agent: "worker",
      kind: "monitor",
      created: 1,
      command: "pnpm test",
      status: "approval",
    };
    expect(backgroundTaskState(task)).toBe("answer");
    expect(backgroundTaskState({ ...task, status: "running" })).toBe("working");
    expect(backgroundTaskState({ ...task, status: "lost" })).toBe("error");
    expect(backgroundTaskState({ ...task, status: "completed" })).toBe(
      "unread",
    );
    expect(backgroundTaskState({ ...task, status: "cancelled" })).toBe(
      "paused",
    );
  });
});

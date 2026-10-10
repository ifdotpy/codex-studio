import { describe, expect, it } from "vitest";
import { createChatSnapshotSelector } from "./snapshotSelection";
import type { Agent, Snapshot } from "../types";

function fixture(): Snapshot {
  const threads: Agent[] = [
    { id: "lead", name: "Lead", source: "managed", isLead: true },
    {
      id: "worker",
      name: "Worker",
      source: "managed",
      rootId: "lead",
      parentId: "lead",
    },
    { id: "other", name: "Other", source: "managed", isLead: true },
  ];
  return {
    token: "token",
    stateDir: "workspace",
    threads,
    nodes: threads,
    chats: [],
    edges: [],
    runtime: {
      agents: threads,
      rooms: [],
      tasks: [],
      monitors: [],
      complaints: [],
      requests: [],
      rules: [],
      projects: [],
      peerTeams: [],
      events: [],
      work: [],
    },
  };
}

function changeAgent(
  data: Snapshot,
  id: string,
  fields: Partial<Agent>,
): Snapshot {
  const threads = data.threads.map((row) =>
    row.id === id ? { ...row, ...fields } : row,
  );
  return {
    ...data,
    threads,
    nodes: threads,
    runtime: { ...data.runtime, agents: threads },
  };
}

describe("chat snapshot selection", () => {
  it("retains the selected tree after unrelated updates but publishes its real changes", () => {
    const select = createChatSnapshotSelector(true);
    const initial = fixture();
    const chosen = select(initial, "lead")!;
    const other = changeAgent(initial, "other", {
      name: "Other updated",
      status: "running",
    });
    expect(select(other, "lead")).toBe(chosen);
    const updated = changeAgent(other, "worker", {
      status: "failed",
      error: "A real failure",
    });
    const next = select(updated, "lead")!;
    expect(next).not.toBe(chosen);
    expect(next.threads.find((row) => row.id === "worker")?.error).toBe(
      "A real failure",
    );
    expect(next.runtime.requests).toBe(chosen.runtime.requests);
    expect(next.runtime.rooms).toBe(chosen.runtime.rooms);
  });

  it("keeps relevant questions and monitors fresh while excluding another tree", () => {
    const select = createChatSnapshotSelector(true);
    const initial = fixture();
    const chosen = select(initial, "lead")!;
    const another = {
      ...initial,
      runtime: {
        ...initial.runtime,
        requests: [{ id: "other-input", agent: "other" }],
      },
    };
    expect(select(another, "lead")).toBe(chosen);
    const question = { id: "selected-input", agent: "worker" };
    const globalQuestion = { id: "global-input" };
    const monitor = {
      id: "monitor",
      agent: "worker",
      status: "running" as const,
    };
    const next = select(
      {
        ...another,
        runtime: {
          ...another.runtime,
          requests: [question, globalQuestion],
          monitors: [monitor],
        },
      },
      "lead",
    )!;
    expect(next.runtime.requests).toEqual([question, globalQuestion]);
    expect(next.runtime.monitors).toEqual([monitor]);
    expect(next).not.toBe(chosen);
  });

  it("updates membership, deletion, selection and workspace changes", () => {
    const select = createChatSnapshotSelector();
    const initial = fixture();
    const chosen = select(initial, "lead")!;
    const moved = changeAgent(initial, "other", {
      isLead: false,
      rootId: "lead",
    });
    expect(select(moved, "lead")!.threads.map((row) => row.id)).toEqual([
      "lead",
      "worker",
      "other",
    ]);
    const removed = {
      ...initial,
      threads: initial.threads.filter((row) => row.id !== "worker"),
    };
    expect(select(removed, "lead")!.threads.map((row) => row.id)).toEqual([
      "lead",
    ]);
    expect(select(initial, "other")!.threads.map((row) => row.id)).toEqual([
      "other",
    ]);
    expect(select(initial)!.threads).toEqual([]);
    expect(select(null, "lead")).toBeNull();
    const reset = select(
      { ...initial, stateDir: "another-workspace" },
      "lead",
    )!;
    expect(reset.stateDir).toBe("another-workspace");
    expect(reset).not.toBe(chosen);
  });

  it("does not suppress credentials or connection notices", () => {
    const select = createChatSnapshotSelector();
    const initial = fixture();
    const chosen = select(initial, "lead")!;
    const next = select(
      {
        ...initial,
        token: "new-token",
        runtime: { ...initial.runtime, connected: false },
      },
      "lead",
    )!;
    expect(next).not.toBe(chosen);
    expect(next.token).toBe("new-token");
    expect(next.runtime.connected).toBe(false);
  });
});

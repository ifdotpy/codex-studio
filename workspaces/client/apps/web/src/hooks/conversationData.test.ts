import { describe, expect, it } from "vitest";
import {
  ChatWaitSource,
  createConversationDataSelector,
} from "./conversationData";
import { createChatSnapshotSelector } from "./snapshotSelection";
import { snapshotProjectionStatusReducer } from "./snapshotProjectionStatus";
import type { Agent, Snapshot } from "../types";

const WORKERS = 512;

function fixture(): Snapshot {
  const lead: Agent = {
    id: "lead",
    name: "Lead",
    source: "managed",
    isLead: true,
    rootId: "lead",
    status: "waiting",
    autoWake: true,
    accountKey: "default",
  };
  const workers: Agent[] = Array.from({ length: WORKERS }, (_, index) => ({
    id: `worker-${index}`,
    name: `Worker ${index}`,
    source: "managed",
    rootId: "lead",
    parentId: "lead",
    status: "completed",
    accountKey: "default",
  }));
  const threads = [lead, ...workers];
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

function changeWorker(
  data: Snapshot,
  index: number,
  fields: Partial<Agent>,
): Snapshot {
  const threads = data.threads.map((row) =>
    row.id === `worker-${index}` ? { ...row, ...fields } : row,
  );
  return {
    ...data,
    threads,
    nodes: threads,
    runtime: { ...data.runtime, agents: threads },
  };
}

// Mirrors the App pipeline. The conversation host is memoized, so an unchanged
// `data` identity is a conversation, composer and transcript render count of 0.
function pipeline() {
  const chat = createChatSnapshotSelector(true);
  const conversation = createConversationDataSelector();
  const source = new ChatWaitSource();
  return (data: Snapshot) => {
    const chosen = chat(data, "lead")!;
    source.set(chosen);
    return {
      conversationData: conversation(chosen),
      wait: source.wait(chosen.threads.find((row) => row.id === "lead")),
    };
  };
}

describe("worker-only snapshot changes", () => {
  it("never change the conversation props across 512 worker status changes", () => {
    const run = pipeline();
    let data = fixture();
    const first = run(data);
    let previous = first;
    let conversationRenders = 0;
    let waitNotifications = 0;
    for (let index = 0; index < WORKERS; index++) {
      data = changeWorker(data, index, { status: "running", inFlight: true });
      const next = run(data);
      if (next.conversationData !== previous.conversationData)
        conversationRenders++;
      if (next.wait !== previous.wait) waitNotifications++;
      previous = next;
    }
    expect(conversationRenders).toBe(0);
    // The lead's wait is real information: each new working agent changes its
    // count, and only the wait leaves render for it.
    expect(waitNotifications).toBe(WORKERS);
    expect(previous.wait?.agents).toHaveLength(WORKERS);
  });

  it("notify no consumer when the wait itself is unchanged", () => {
    const run = pipeline();
    let data = changeWorker(fixture(), 0, {
      status: "running",
      inFlight: true,
    });
    const first = run(data);
    for (let count = 1; count <= WORKERS; count++) {
      data = changeWorker(data, 1, { tokensUsed: count, status: "completed" });
      const next = run(data);
      expect(next.conversationData).toBe(first.conversationData);
      expect(next.wait).toBe(first.wait);
    }
  });

  it("render the conversation for a name or error a team member shows", () => {
    const run = pipeline();
    const data = fixture();
    const first = run(data).conversationData;
    expect(
      run(changeWorker(data, 3, { name: "Renamed" })).conversationData,
    ).not.toBe(first);
    expect(
      run(changeWorker(data, 3, { error: "Failed" })).conversationData,
    ).not.toBe(first);
  });

  it("keep a repeated projection report from scheduling a render", () => {
    const initial = {
      startupError: "",
      projectionError: "",
      transportError: "",
    };
    for (const action of [
      { type: "data-received" },
      { type: "projection-recovered" },
      { type: "startup-recovered" },
      { type: "transport-changed", error: "" },
    ] as const)
      expect(snapshotProjectionStatusReducer(initial, action)).toBe(initial);
    const failed = snapshotProjectionStatusReducer(initial, {
      type: "projection-failed",
      error: "x",
    });
    expect(failed).not.toBe(initial);
    expect(
      snapshotProjectionStatusReducer(failed, {
        type: "projection-failed",
        error: "x",
      }),
    ).toBe(failed);
  });
});

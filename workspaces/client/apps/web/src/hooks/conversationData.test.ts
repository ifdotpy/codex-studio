import { describe, expect, it } from "vitest";
import {
  ChatWaitSource,
  createConversationDataSelector,
} from "./conversationData";
import { createChatSnapshotSelector } from "./snapshotSelection";
import { snapshotProjectionStatusReducer } from "./snapshotProjectionStatus";
import type { Agent, Snapshot } from "../types";

const WORKERS = 512;

function fixture(workerCount = WORKERS): Snapshot {
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
  const workers: Agent[] = Array.from({ length: workerCount }, (_, index) => ({
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
      studioErrors: [],
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

// Exercise the selectors used by App; identity changes are not React render counts.
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
  it.each([8, WORKERS])(
    "retains conversation props across worker status changes in a %i-worker team",
    (count) => {
      const run = pipeline();
      let data = fixture(count);
      const first = run(data);
      let previousWait = first.wait;
      const indices =
        count === WORKERS
          ? [0, WORKERS / 2, WORKERS - 1]
          : Array.from({ length: count }, (_, i) => i);
      for (const [offset, index] of indices.entries()) {
        data = changeWorker(data, index, { status: "running", inFlight: true });
        const next = run(data);
        expect(next.conversationData).toBe(first.conversationData);
        expect(next.wait?.agents).toHaveLength(offset + 1);
        expect(next.wait).not.toBe(previousWait);
        previousWait = next.wait;
      }
    },
  );

  it("retains wait identity across token-only changes", () => {
    const run = pipeline();
    let data = changeWorker(fixture(), 0, {
      status: "running",
      inFlight: true,
    });
    const first = run(data);
    for (let count = 1; count <= 3; count++) {
      data = changeWorker(data, 1, {
        lastEvent: `2026-10-24T00:00:0${count}Z`,
        status: "completed",
      });
      const next = run(data);
      expect(next.conversationData).toBe(first.conversationData);
      expect(next.wait).toBe(first.wait);
    }
  });

  it("changes conversation identity for a team member name or error", () => {
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

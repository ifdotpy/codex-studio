import type { Agent } from "../../types";
import type { ChatIndicator } from "../chat-status/chatStatusModel";
import assert from "node:assert/strict";
import { it } from "vitest";
import { awaitingAnswerIds, workerState } from "./WorkerOverview.tsx";

it("only includes pending requests with a string agent identity", () => {
  assert.deepEqual(
    [
      ...awaitingAnswerIds([
        { id: "valid", agent: "worker", status: "pending" },
        { id: "missing", status: "pending" },
        { id: "answered", agent: "other", status: "answered" },
        { id: "deferred", agent: "later", status: "pending", deferred: true },
      ]),
    ],
    ["worker"],
  );
});

it("keeps unknown worker statuses in the existing waiting fallback", () => {
  assert.equal(
    workerState({ id: "worker", status: null }, new Set()),
    "waiting",
  );
  assert.equal(
    workerState({ id: "worker", status: "paused" }, new Set()),
    "stopped",
  );
});

it("orders actionable indicators before work and keeps completed results collapsed", async () => {
  const { teamDispatchGroups } = await import("./WorkerOverview.tsx");
  const rows: Agent[] = [
    { id: "work", status: "running" },
    { id: "answer", status: "running", inFlight: true },
    { id: "failed", status: "failed" },
    { id: "unread", status: "completed" },
    { id: "read", status: "completed" },
    { id: "paused", status: "paused" },
  ];
  const indicators = new Map<string, ChatIndicator>([
    ["work", { kind: "working", label: "Working" }],
    ["answer", { kind: "answer", label: "Needs your answer" }],
    ["failed", { kind: "error", label: "Failed" }],
    ["unread", { kind: "unread", label: "Unread result" }],
    ["read", { kind: "none", label: "Read" }],
    ["paused", { kind: "paused", label: "Stopped" }],
  ]);
  const { groups, completed } = teamDispatchGroups(rows, indicators);
  assert.deepEqual(
    groups.filter((group) => group.workers.length).map((group) => group.kind),
    ["answer", "error", "working", "paused"],
  );
  assert.deepEqual(
    completed.map((row) => row.id),
    ["unread", "read"],
  );
  assert.equal(
    groups[0]?.workers[0]?.id,
    "answer",
    "an in-flight question stays actionable",
  );
});

it("derives hierarchy from parent identities and terminates on cycles", async () => {
  const { workerDepth } = await import("./WorkerOverview.tsx");
  const agents = new Map<string, Agent>([
    ["lead", { id: "lead" }],
    ["worker", { id: "worker", parentId: "lead" }],
    ["reviewer", { id: "reviewer", parentId: "worker" }],
  ]);
  assert.equal(
    workerDepth(agents.get("lead") ?? assert.fail("Missing lead"), agents),
    0,
  );
  assert.equal(
    workerDepth(
      agents.get("reviewer") ?? assert.fail("Missing reviewer"),
      agents,
    ),
    2,
  );
  agents.set("lead", { id: "lead", parentId: "reviewer" });
  assert.equal(
    workerDepth(
      agents.get("reviewer") ?? assert.fail("Missing reviewer"),
      agents,
    ),
    2,
  );
});

it("keeps parents before their children without losing orphaned or cyclic rows", async () => {
  const { orderWorkerHierarchy } = await import("./WorkerOverview.tsx");
  const agents: Agent[] = [
    { id: "child", parentId: "parent" },
    { id: "other" },
    { id: "parent" },
    { id: "cycle-a", parentId: "cycle-b" },
    { id: "cycle-b", parentId: "cycle-a" },
  ];
  const ordered = orderWorkerHierarchy(agents).map((agent) => agent.id);
  assert.deepEqual(ordered, ["other", "parent", "child", "cycle-a", "cycle-b"]);
});

it("filters the same canonical lanes counted by the summary, including queued work", async () => {
  const { filterTeamWorkers } = await import("./WorkerOverview.tsx");
  const rows: Agent[] = [
    { id: "queued", status: "queued" },
    { id: "running", status: "running", inFlight: true },
    { id: "error", status: "failed" },
    { id: "paused", status: "paused" },
    { id: "done", status: "completed" },
  ];
  const indicators = new Map<string, ChatIndicator>([
    ["queued", { kind: "working", label: "Queued" }],
    ["running", { kind: "working", label: "Working" }],
    ["error", { kind: "error", label: "Failed" }],
    ["paused", { kind: "paused", label: "Stopped" }],
    ["done", { kind: "unread", label: "New result" }],
  ]);
  const ids = (filter: string) =>
    filterTeamWorkers(rows, indicators, filter).map((a) => a.id);
  assert.deepEqual(ids("active"), ["queued", "running"]);
  assert.deepEqual(ids("attention"), ["error"]);
  assert.deepEqual(ids("paused"), ["paused"]);
  assert.deepEqual(ids("completed"), ["done"]);
  assert.strictEqual(filterTeamWorkers(rows, indicators, "all"), rows);
});

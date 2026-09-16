import assert from "node:assert/strict";
import { desktopAlerts } from "../web/src/desktopAlerts.ts";

const lead = {
  id: "lead",
  name: "Main",
  isLead: true,
  status: "completed",
  lastCompletedTurn: "turn-1",
  lastCompletedTurnStatus: "completed",
  tail: "The checks pass.",
};
const worker = { ...lead, id: "worker", isLead: false };
const data = {
  threads: [lead, worker],
  runtime: {
    requests: [
      {
        id: "question",
        agent: "lead",
        status: "pending",
        params: { questions: [{ question: "Which scope?" }] },
      },
      { id: "approval", agent: "worker", status: "pending" },
      { id: "deferred", agent: "lead", status: "pending", deferred: true },
      { id: "answered", agent: "lead", status: "answered" },
    ],
    complaints: [
      {
        id: "for-user",
        leadId: "lead",
        recipient: "user",
        needsResponse: true,
        title: "Please check the result.",
        version: 2,
      },
      {
        id: "for-lead",
        leadId: "lead",
        recipient: "lead",
        needsResponse: true,
        title: "Internal blocker",
      },
    ],
    userTasks: [
      { id: "task", agent: "lead", status: "open", title: "Check the package" },
    ],
  },
};
const alerts = desktopAlerts(data);
assert.deepEqual(
  alerts.map((a) => a.id),
  [
    "completed:lead:turn-1",
    "request:question",
    "request:approval",
    "task:task",
    "complaint:for-user:2",
  ],
);
assert.equal(alerts[0].body, lead.tail);
assert.equal(alerts[1].body, "Which scope?");
assert.equal(alerts[2].target.agentId, "worker");
for (const status of ["running", "waiting", "paused", "queued"]) {
  assert.equal(
    desktopAlerts({ ...data, threads: [{ ...lead, status }] }).some((a) =>
      a.id.startsWith("completed:"),
    ),
    false,
  );
}
assert.equal(
  desktopAlerts({ ...data, threads: [{ ...lead, inFlight: true }] }).some((a) =>
    a.id.startsWith("completed:"),
  ),
  false,
);
assert.equal(
  desktopAlerts({ ...data, threads: [{ ...lead, deletedAt: 1 }] }).length,
  0,
);
const failed = desktopAlerts({
  ...data,
  threads: [
    { ...lead, status: "failed", error: { message: "Request failed" } },
  ],
});
assert.equal(failed[0].body, "Request failed");
assert.equal(failed[0].id, "failed:lead:turn-1");
console.log("Desktop alert classification passed.");

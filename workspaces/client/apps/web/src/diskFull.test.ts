import { snapshotFixture as snapshot } from "./snapshotFixture";
import type { Agent } from "./types";
// A turn that failed because the disk was full shows a held agent, not a failure.
import assert from "node:assert/strict";
import { it } from "vitest";
import { DISK_FULL_WAIT_LABEL, diskFullHold } from "./capacityRetry.ts";
import { chatIndicators } from "./components/chat-status/chatStatusModel.ts";
import { isDiskFullError, nativeErrorView } from "./nativeErrors.ts";

const INCIDENT =
  "ENOSPC: no space left on device, rename '/state/account-servers/claude-profile-ee03/sessions/55f43293.meta.json.7c1e.tmp' -> '/state/account-servers/claude-profile-ee03/sessions/55f43293.meta.json'";

const held = (overrides: Partial<Agent> = {}) =>
  ({
    id: "lead",
    rootId: "lead",
    name: "Lead",
    source: "managed",
    isLead: true,
    status: "failed",
    autoWake: true,
    epoch: 2,
    threadId: "thread",
    accountKey: "default",
    lastCompletedTurn: "turn",
    lastCompletedTurnStatus: "failed",
    error: { message: INCIDENT },
    capacityRetry: {
      dueAt: null,
      updatedAt: 0,
      id: "retry",
      threadId: "thread",
      epoch: 2,
      accountKey: "default",
      cause: "diskFull",
      status: "scheduled",
      reason:
        "Disk is full: 130 MiB free. Studio resumes this agent when free space returns.",
    },
    ...overrides,
  }) satisfies Agent;

it("recognizes disk-full errors from providers and Studio", () => {
  for (const value of [
    { message: INCIDENT },
    INCIDENT,
    JSON.stringify({ message: "database or disk is full" }),
    { message: "Write failed", codexErrorInfo: "diskFull" },
    { message: "Disk is full.", data: { diskFull: true } },
  ])
    assert.equal(isDiskFullError(value), true, JSON.stringify(value));
  for (const value of [
    { message: "Model is at capacity", codexErrorInfo: "serverOverloaded" },
    "permission denied",
    null,
  ])
    assert.equal(isDiskFullError(value), false, JSON.stringify(value));
});

it("shows a clear disk-full message instead of the raw ENOSPC text", () => {
  const view = nativeErrorView({ message: INCIDENT });
  assert.equal(view.title, "Disk is full");
  assert.equal(
    view.message,
    "Studio holds this agent and resumes it when free space returns.",
  );
  assert.equal(view.severity, "warning");
  assert.ok(view.details.includes("ENOSPC"), "the raw error stays in details");
});

it("presents a disk-full hold as waiting, not failed", () => {
  const agent = held();
  assert.ok(agent.capacityRetry);
  assert.equal(diskFullHold(agent), agent.capacityRetry);
  assert.deepEqual(chatIndicators(snapshot([agent])).get("lead"), {
    kind: "paused",
    label: DISK_FULL_WAIT_LABEL,
  });
  const other = held({
    capacityRetry: { ...agent.capacityRetry, cause: "serverOverloaded" },
  });
  assert.equal(diskFullHold(other), null);
  assert.deepEqual(chatIndicators(snapshot([other])).get("lead"), {
    kind: "error",
    label: "Failed",
  });
  const finished = held({
    capacityRetry: { ...agent.capacityRetry, status: "exhausted" },
  });
  assert.equal(diskFullHold(finished), null);
});

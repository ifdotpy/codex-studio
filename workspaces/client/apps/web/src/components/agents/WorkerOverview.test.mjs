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

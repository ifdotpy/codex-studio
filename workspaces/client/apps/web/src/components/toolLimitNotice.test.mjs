import assert from "node:assert/strict";
import { it } from "vitest";
import { toolLimitNotice } from "./toolLimitNotice.ts";

it("shows limits only for failed worker results and retains the worker label", () => {
  assert.deepEqual(
    toolLimitNotice({
      text: JSON.stringify({
        status: "failed",
        agent_id: "worker-1",
        name: "Test worker",
        result: {
          error: {
            codexErrorInfo: "usageLimitExceeded",
            message: "Usage limit reached; try again at 2026-10-05 10:00 UTC",
          },
        },
      }),
      toolStatus: "failed",
    }),
    {
      title: "Worker stopped: usage limit reached",
      message: "Test worker · Try again at 2026-10-05 10:00 UTC.",
    },
  );
  assert.equal(
    toolLimitNotice({
      text: JSON.stringify({
        status: "completed",
        error: { codexErrorInfo: "usageLimitExceeded" },
      }),
      toolStatus: "completed",
    }),
    null,
  );
});

it("ignores malformed nested provider values without throwing", () => {
  assert.equal(
    toolLimitNotice({
      text: JSON.stringify({
        status: "failed",
        contentItems: [null, [], "text", { text: { unexpected: true } }],
        result: ["not", "an object"],
      }),
      toolStatus: "failed",
    }),
    null,
  );
});

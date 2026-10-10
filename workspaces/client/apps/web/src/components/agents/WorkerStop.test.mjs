import assert from "node:assert/strict";
import { it } from "vitest";
import { agentStopReason } from "../../types.ts";

it("identifies an explicit user or parent stop", () => {
  for (const error of ["Stopped by user", "Stopped by agent Release lead"])
    assert.equal(
      agentStopReason({ status: "paused", autoWake: false, error }),
      error,
    );
});

it("preserves errors without an exact explicit stop", () => {
  for (const error of [
    "Codex app-server is offline",
    "Stopped by agent ",
    "Stopped by user after an error",
    { message: "Stopped by user", codexErrorInfo: "other" },
    JSON.stringify({ message: "Stopped by user" }),
  ])
    assert.equal(
      agentStopReason({ status: "paused", autoWake: false, error }),
      "",
    );
  for (const state of [
    { status: "failed", autoWake: false },
    { status: "interrupted", autoWake: false },
    { status: "paused", autoWake: true },
    { status: "paused" },
  ])
    assert.equal(agentStopReason({ ...state, error: "Stopped by user" }), "");
});

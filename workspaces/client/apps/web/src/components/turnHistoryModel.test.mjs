import assert from "node:assert/strict";
import { it } from "vitest";
import { historyGroups } from "./turnHistoryModel.ts";

it("does not create a failed turn group for an empty turn ID", () => {
  const [group] = historyGroups([
    {
      id: "failure-without-turn",
      role: "assistant",
      text: "",
      turnId: "",
      turnStatus: "failed",
    },
  ]);

  assert.equal(group.outcome, undefined);
});

it("keeps an unconfirmed outcome terminal even with a stale active tool", () => {
  const [group] = historyGroups(
    [
      {
        id: "tool",
        role: "tool",
        text: "command",
        turnId: "turn",
        turnStatus: "uncertain",
        toolStatus: "running",
      },
      {
        id: "answer",
        role: "assistant",
        text: "Partial reply",
        turnId: "turn",
        turnStatus: "uncertain",
      },
    ],
    "turn",
  );
  assert.equal(group.outcome, "uncertain");
});

it("does not convert an unconfirmed outcome to failed or completed", () => {
  for (const status of ["completed", "failed", "interrupted"]) {
    const [group] = historyGroups([
      {
        id: "answer",
        role: "assistant",
        text: "Partial reply",
        turnId: "turn",
        turnStatus: status,
      },
      {
        id: "notice",
        role: "system",
        text: "No proof",
        turnId: "turn",
        turnStatus: "uncertain",
      },
    ]);
    assert.equal(group.outcome, "uncertain");
  }
});

it("shows an unconfirmed turn that has no assistant reply", () => {
  const [group] = historyGroups([
    {
      id: "input",
      role: "user",
      text: "Run the command",
      turnId: "turn",
      turnStatus: "uncertain",
    },
  ]);
  assert.equal(group.outcome, "uncertain");
});

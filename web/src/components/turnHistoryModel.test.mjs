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

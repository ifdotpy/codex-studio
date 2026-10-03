import assert from "node:assert/strict";
import { accountLimits } from "./accountUsage.ts";

import { it } from "vitest";

it("returns limits only for the selected account and matching native identity", () => {
  const a = { accountKey: "a", data: { accountId: "native-a" } };
  assert.equal(accountLimits(a, "a", "native-a"), a);
  assert.equal(accountLimits(a, "b"), null);
  assert.equal(accountLimits(a, "a", "replacement-login"), null);
  assert.equal(accountLimits({ data: {} }, "a"), null);
  assert.equal(accountLimits({ data: {} }, "default"), null);
  assert.equal(accountLimits(null, "default"), null);
  console.log("Account limits ownership: 6 passed");
});

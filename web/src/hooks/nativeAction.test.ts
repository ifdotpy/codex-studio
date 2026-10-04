import assert from "node:assert/strict";
import { it } from "vitest";
import { ApiError, NetworkTimeoutError } from "../api";
import { isDefinitivelyNotApplied } from "../useNativeAction";

it("clears a saved action only after an explicit not_applied receipt", () => {
  assert.equal(
    isDefinitivelyNotApplied(
      new ApiError("The action was not applied.", 409, {
        outcome: "not_applied",
      }),
    ),
    true,
  );
  assert.equal(
    isDefinitivelyNotApplied(
      new ApiError("The result is uncertain.", 500, { outcome: "uncertain" }),
    ),
    false,
  );
  assert.equal(isDefinitivelyNotApplied(new NetworkTimeoutError()), false);
});

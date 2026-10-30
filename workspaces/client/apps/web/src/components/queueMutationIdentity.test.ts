import { describe, expect, it } from "vitest";
import { queueMutationMessageId } from "./queueMutationIdentity";

describe("queueMutationMessageId", () => {
  it("returns the target id", () => {
    expect(queueMutationMessageId({ action: "cancel", id: "target" })).toBe(
      "target",
    );
  });

  it("does not infer an identity for a reorder", () => {
    expect(
      queueMutationMessageId({
        action: "reorder",
        ordered_ids: ["one"],
        expected_revision: "revision-1",
      }),
    ).toBeUndefined();
  });
});

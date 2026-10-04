import { describe, expect, it } from "vitest";
import { queueMutationMessageId } from "./queueMutationIdentity";

describe("queueMutationMessageId", () => {
  it("accepts both generated and legacy queue operation identities", () => {
    expect(
      queueMutationMessageId({ action: "cancel", message_id: "wire-id" }),
    ).toBe("wire-id");
    expect(
      queueMutationMessageId({
        action: "edit",
        id: "legacy-id",
        text: "updated",
      }),
    ).toBe("legacy-id");
  });

  it("uses the message identity when both forms are present", () => {
    expect(
      queueMutationMessageId({
        action: "first",
        message_id: "wire-id",
        id: "legacy-id",
      }),
    ).toBe("wire-id");
  });

  it("does not infer a message identity for queue-wide reorder", () => {
    expect(
      queueMutationMessageId({
        action: "reorder",
        ordered_ids: ["one"],
        expected_revision: "revision-1",
      }),
    ).toBeUndefined();
  });
});

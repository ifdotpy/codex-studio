import { describe, expect, it } from "vitest";
import { legacyComplaintNeedsUserResponse } from "../hooks";

describe("legacy complaint response projection", () => {
  it("marks only user-recipient complaints that need a response", () => {
    expect(
      legacyComplaintNeedsUserResponse({
        recipient: "user",
        needsResponse: true,
      }),
    ).toBe(true);
    expect(
      legacyComplaintNeedsUserResponse({
        recipient: "lead",
        needsResponse: true,
      }),
    ).toBe(false);
    expect(
      legacyComplaintNeedsUserResponse({
        recipient: "user",
        needsResponse: false,
      }),
    ).toBe(false);
  });
});

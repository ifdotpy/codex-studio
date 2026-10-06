import { describe, expect, it } from "vitest";
import { snapshotErrorMessage } from "../hooks";

describe("snapshot projection errors", () => {
  it("surfaces a first-load projection failure in the startup alert", () => {
    expect(snapshotErrorMessage("Entity database is unavailable", "", "")).toBe(
      "Entity database is unavailable",
    );
  });

  it("surfaces a projection subscription failure in the app alert", () => {
    expect(
      snapshotErrorMessage(
        "",
        "Entity pull failed",
        "Live updates are reconnecting.",
      ),
    ).toBe("Entity pull failed");
  });
});

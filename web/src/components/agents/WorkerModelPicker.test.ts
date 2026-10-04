import { describe, expect, it } from "vitest";
import { supportsDaybreakMode } from "./WorkerModelPicker";

describe("supportsDaybreakMode", () => {
  it("keeps standard mode available when the cyber catalog field is absent", () => {
    expect(supportsDaybreakMode({ model: "claude-sonnet" }, false)).toBe(true);
    expect(
      supportsDaybreakMode(
        { model: "claude-sonnet", availableAccessPrograms: null },
        false,
      ),
    ).toBe(true);
  });

  it("requires explicit standard access when cyber metadata is present", () => {
    expect(
      supportsDaybreakMode(
        {
          model: "claude-sonnet",
          availableAccessPrograms: { cyber: ["standard"] },
        },
        false,
      ),
    ).toBe(true);
    expect(
      supportsDaybreakMode(
        {
          model: "claude-sonnet",
          availableAccessPrograms: { cyber: ["daybreakBlue"] },
        },
        false,
      ),
    ).toBe(false);
  });

  it.each([null, "standard", { unexpected: true }])(
    "does not grant standard mode for malformed cyber metadata: %s",
    (cyber) => {
      expect(
        supportsDaybreakMode(
          {
            model: "claude-sonnet",
            availableAccessPrograms: { cyber },
          },
          false,
        ),
      ).toBe(false);
    },
  );
});

import { describe, expect, it } from "vitest";
import { accountDisplayName, accountTooltip } from "./accountName";

describe("account names", () => {
  it("uses the persisted label and the full email tooltip", () => {
    const account = { label: " Work 1 ", email: "work@example.com" };
    expect(accountDisplayName(account)).toBe("Work 1");
    expect(accountTooltip(account)).toBe("work@example.com");
  });
  it("uses the email prefix after the label is cleared", () => {
    expect(accountDisplayName({ label: "", email: "work@example.com" })).toBe(
      "work",
    );
    expect(accountDisplayName({ label: "  ", email: "work@example.com" })).toBe(
      "work",
    );
    expect(accountDisplayName({ label: "", email: null })).toBe("Account");
    expect(accountTooltip({ email: null })).toBe("");
  });
});

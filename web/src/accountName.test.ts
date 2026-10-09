import { describe, expect, it } from "vitest";
import {
  accountDisplayName,
  accountTooltip,
  accountCanStart,
} from "./accountName";

describe("account names", () => {
  it("allows a pinned native proof attempt without claiming authentication", () => {
    const account = {
      id: "claude-local",
      home: "/profile",
      label: "Work",
      source: "Claude",
      provider: "claude" as const,
      status: "error" as const,
      canAttemptNativeProof: true,
    };
    expect(accountCanStart(account)).toBe(true);
    expect(account.status).toBe("error");
    expect(accountCanStart({ ...account, canAttemptNativeProof: false })).toBe(
      false,
    );
    expect(accountCanStart({ ...account, status: "signedOut" })).toBe(false);
    expect(accountCanStart({ ...account, provider: "codex" })).toBe(false);
    expect(accountCanStart({ ...account, disconnected: true })).toBe(false);
    expect(accountCanStart({ ...account, deleted: true })).toBe(false);
  });
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
  it("shows only the part before @ when the label is an email", () => {
    expect(
      accountDisplayName({
        label: "ifdotpy@gmail.com",
        email: "ifdotpy@gmail.com",
      }),
    ).toBe("ifdotpy");
  });
});

import { describe, expect, it } from "vitest";
import {
  codeVerificationPending,
  currentClaudeLoginReadMatches,
} from "./claudeCodeSubmission";

describe("Claude authorization code submission", () => {
  it("keeps the form available until the server confirms submission", () => {
    expect(codeVerificationPending({ status: "pending" })).toBe(false);
    expect(
      codeVerificationPending({ status: "pending", codeSubmitted: false }),
    ).toBe(false);
  });

  it("waits for verification after the server records code submission", () => {
    expect(
      codeVerificationPending({ status: "pending", codeSubmitted: true }),
    ).toBe(true);
    expect(
      codeVerificationPending({ status: "error", codeSubmitted: true }),
    ).toBe(false);
  });

  it("ignores an old login response after the current flow changes", () => {
    expect(
      currentClaudeLoginReadMatches(
        "new-request",
        "old-request",
        "old-request",
        1,
        1,
      ),
    ).toBe(false);
    expect(
      currentClaudeLoginReadMatches(
        "current",
        "current",
        "other-response",
        1,
        1,
      ),
    ).toBe(false);
    expect(
      currentClaudeLoginReadMatches("current", "current", "current", 2, 1),
    ).toBe(false);
    expect(
      currentClaudeLoginReadMatches("current", "current", "current", 2, 2),
    ).toBe(true);
  });
});

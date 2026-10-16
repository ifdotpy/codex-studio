import { describe, expect, it } from "vitest";
import { ApiError } from "../api";
import {
  codeVerificationPending,
  codeSubmissionNotApplied,
  codeSubmissionResolved,
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

  it("does not resolve an uncertain submission from a pending false status", () => {
    expect(
      codeSubmissionResolved({ status: "pending", codeSubmitted: false }),
    ).toBe(false);
    expect(codeSubmissionResolved({ status: "pending" })).toBe(false);
    expect(
      codeSubmissionResolved({ status: "pending", codeSubmitted: true }),
    ).toBe(true);
    expect(codeSubmissionResolved({ status: "error" })).toBe(true);
    expect(codeSubmissionResolved({ status: "cancelled" })).toBe(true);
  });

  it("allows correction only after the server says the write was not applied", () => {
    expect(
      codeSubmissionNotApplied(
        new ApiError("Enter the final authorization response", 400, {
          outcome: "not_applied",
        }),
      ),
    ).toBe(true);
    expect(
      codeSubmissionNotApplied(
        new ApiError("Outcome unknown", 500, { outcome: "outcome_unknown" }),
      ),
    ).toBe(false);
    expect(codeSubmissionNotApplied(new Error("Network error"))).toBe(false);
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

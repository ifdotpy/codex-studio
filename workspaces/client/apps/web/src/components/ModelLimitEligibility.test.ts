import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { expect, test } from "vitest";
import ModelLimitEligibility, {
  type UsageEligibility,
} from "./ModelLimitEligibility";
import AccountSignInNotice from "./AccountSignInNotice";
import ClaudeProfile from "./ClaudeProfile";
import type { Account } from "./Accounts";

const eligibility: UsageEligibility = {
  agentId: "a",
  accountKey: "default",
  model: "sonnet",
  provider: "claude",
  allowed: false,
  canRun: false,
  fresh: true,
  checkedAt: 1000,
  validUntil: 1005,
  resetAt: 1300,
  buckets: [
    { id: "claude", overageAllowed: false },
    { id: "claude-sonnet", overageAllowed: false },
  ],
  overage: "rejected",
};
const render = (element: ReturnType<typeof createElement>) =>
  renderToStaticMarkup(createElement(MantineProvider, null, element));
test("model view shows Rust eligibility, applicable buckets, reset and overage", () => {
  for (const [overage, label] of [
    ["allowed", "Overage allowed"],
    ["in_use", "Overage in use"],
    ["rejected", "Overage rejected"],
  ] as const) {
    const html = render(
      createElement(ModelLimitEligibility, {
        eligibility: { ...eligibility, overage },
        now: 1000,
      }),
    );
    expect(html).toContain("Cannot run now");
    expect(html).toContain("claude-sonnet");
    expect(html).not.toContain("claude-opus");
    expect(html).toContain(label);
    expect(html).toContain("1970-01-01T00:21:40.000Z");
  }
  expect(
    render(
      createElement(ModelLimitEligibility, {
        eligibility: { ...eligibility, canRun: true },
        now: 1000,
      }),
    ),
  ).toContain("Can run now");
  expect(
    render(
      createElement(ModelLimitEligibility, {
        eligibility: { ...eligibility, canRun: true },
        now: 1006,
      }),
    ),
  ).toContain("Current eligibility is unknown");
});
const expected = { name: "Expected organization", idSuffix: "12345678" };
const observed = { name: "Observed organization", idSuffix: "87654321" };
const account: Account = {
  id: "claude",
  label: "Claude",
  status: "changed",
  provider: "claude",
  home: "/fixture",
  source: "Fixture",
  organization: expected,
  organizationMismatch: { expected, observed },
};
test("organization mismatch keeps the existing sign-in action and profile pin", () => {
  const html = render(
    createElement(AccountSignInNotice, {
      account,
      errors: [],
      onSignIn: () => {},
    }),
  );
  expect(html).toContain(
    "Signed in to a different organization: Observed organization (87654321). This profile requires Expected organization (12345678).",
  );
  expect(html).toContain("Sign in to Claude");
  const profile = render(
    createElement(ClaudeProfile, {
      account,
      onSaved: () => {},
      expanded: true,
    }),
  );
  expect(profile).toContain("Expected organization (12345678)");
  expect(profile).not.toContain("Observed organization");
});

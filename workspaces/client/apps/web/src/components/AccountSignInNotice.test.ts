import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { expect, test } from "vitest";
import AccountSignInNotice from "./AccountSignInNotice";
import ClaudeProfile from "./ClaudeProfile";
import type { Account } from "./Accounts";

const render = (element: ReturnType<typeof createElement>) =>
  renderToStaticMarkup(createElement(MantineProvider, null, element));
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

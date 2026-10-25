import { createElement, type ComponentProps } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { beforeEach, expect, it, vi } from "vitest";
import type { ActionButtonProps } from "../components/ui/primitives";
import type { ServerAccount } from "./navigation";
import ServerAccountsPanel, { accountSources } from "./ServerAccountsPanel";

const controls = vi.hoisted(() => [] as ActionButtonProps[]);
vi.mock("../components/ui/primitives", async (original) => {
  const actual = await original<typeof import("../components/ui/primitives")>();
  return {
    ...actual,
    ActionButton: (props: ActionButtonProps) => {
      controls.push(props);
      return createElement(actual.ActionButton, props);
    },
  };
});

const local = {
  id: "local",
  label: "Workstation",
  origin: "https://local.test",
};
const remote = {
  id: "remote",
  label: "Build server",
  origin: "https://remote.test",
};
const account = {
  accountKey: "profile-exact",
  provider: "claude",
  label: "Work",
  email: "work@example.test",
  plan: "Max",
  status: "ready",
  isDefault: false,
} satisfies ServerAccount & { accountKey: string };
const render = (
  overrides: Partial<ComponentProps<typeof ServerAccountsPanel>> = {},
) => {
  const startAccount = vi.fn();
  const html = renderToStaticMarkup(
    createElement(
      MantineProvider,
      null,
      createElement(ServerAccountsPanel, {
        servers: [local, remote],
        accountsByServer: { local: [account] },
        statuses: { local: "live", remote: "offline" },
        startAccount,
        accountAction: () => {},
        ...overrides,
      }),
    ),
  );
  return { html, startAccount };
};
beforeEach(() => {
  controls.length = 0;
});

it("does not treat a local fallback record as a remote account source", () => {
  const row = {
    key: "claude:work@example.test",
    provider: "claude" as const,
    label: account.label,
    email: account.email,
    plan: account.plan,
    byServer: { remote: account },
  };
  const cached = {
    ...account,
    id: account.accountKey,
    home: "/fixture",
    source: "fixture",
    status: "ready" as const,
  };
  expect(
    accountSources(row, [remote], { remote: [cached] }, () => undefined),
  ).toEqual([]);
  const { html } = render({
    accountsByServer: { remote: [account] },
    servicesFor: () => undefined,
  });
  expect(html).toContain("Build server: Connection unknown");
  const login = controls.find(
    (button) => button["aria-label"] === "Sign in to Work on Build server",
  );
  expect(login?.disabled).toBe(true);
});

it("shows an exact-account sign-in action without opening account details", () => {
  const { html, startAccount } = render();
  expect(html).toContain('aria-expanded="false"');
  expect(html).toContain('aria-label="Sign in to Work on Workstation"');
  expect(html).toContain("Sign in");
  const login = controls.find(
    (button) => button["aria-label"] === "Sign in to Work on Workstation",
  );
  expect(login).toBeDefined();
  expect(login?.disabled).not.toBe(true);
  login?.onClick?.(
    {} as Parameters<NonNullable<ActionButtonProps["onClick"]>>[0],
  );
  expect(startAccount).toHaveBeenCalledExactlyOnceWith("local", {
    action: "account-sign-in",
    accountKey: "profile-exact",
    provider: "claude",
    email: account.email,
    label: account.label,
    serverLabel: local.label,
  });
});

it("does not send a login from an older row before its exact account identity is known", () => {
  const { accountKey: _key, ...legacy } = account;
  const { startAccount } = render({ accountsByServer: { local: [legacy] } });
  const login = controls.find(
    (button) => button["aria-label"] === "Sign in to Work on Workstation",
  );
  expect(login).toBeDefined();
  expect(login?.disabled).toBe(true);
  login?.onClick?.(
    {} as Parameters<NonNullable<ActionButtonProps["onClick"]>>[0],
  );
  expect(startAccount).not.toHaveBeenCalled();
});

it("does not mark a local account offline because an unrelated peer is offline", () => {
  const { html } = render();
  expect(html).not.toContain('title="Offline"');
  expect(html).not.toContain("Build server: Offline");
});

it.each([undefined, "idle", "connecting"])(
  "does not report unknown or %s transport as offline",
  (status) => {
    const { html } = render({ statuses: status ? { local: status } : {} });
    expect(html).not.toContain('title="Offline"');
    expect(html).not.toContain("Workstation: Offline");
    expect(
      controls.find(
        (button) => button["aria-label"] === "Sign in to Work on Workstation",
      )?.disabled,
    ).not.toBe(true);
  },
);

it.each([
  ["offline", "Workstation: Offline"],
  ["degraded", "Workstation: Connection degraded"],
  ["schema-mismatch", "Workstation: Update required"],
])("names the server and the exact %s transport condition", (status, label) => {
  const { html } = render({ statuses: { local: status } });
  expect(html).toContain(label);
  const login = controls.find(
    (button) => button["aria-label"] === "Sign in to Work on Workstation",
  );
  expect(login).toBeDefined();
  expect(login?.disabled).toBe(status !== "degraded");
});

it("keeps sign-in needed visible without treating a saved plan as current authentication", () => {
  const { html } = render({
    accountsByServer: { local: [{ ...account, status: "signedOut" }] },
    statuses: { local: "live" },
  });
  expect(html).toContain("Sign in needed");
  expect(html).toContain('aria-label="Sign in to Work on Workstation"');
  expect(html).not.toContain("Signed in");
});

it.each([
  ["error", "Sign-in check failed"],
  ["unknown", "Sign-in status unknown"],
])("does not call an %s authentication result signed out", (status, label) => {
  const { html } = render({
    accountsByServer: { local: [{ ...account, status }] },
    statuses: { local: "live" },
  });
  expect(html).toContain(`Workstation: ${label}`);
  expect(html).not.toContain("Sign in needed");
});

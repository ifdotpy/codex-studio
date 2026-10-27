import { ProviderKind } from "../generated/providerKind";
import {
  createElement,
  Fragment,
  type ComponentProps,
  type ReactNode,
} from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { beforeEach, expect, it, vi } from "vitest";
import type { ActionButtonProps } from "../components/ui/primitives";
import type { ServerAccount } from "./navigation";
import ServerAccountsPanel, { accountSources } from "./ServerAccountsPanel";
import { serverAccountRows } from "./serverAccountRows";
import { required } from "../components/componentTestFixtures";

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

const menuItems = vi.hoisted(
  () =>
    [] as { children?: unknown; disabled?: boolean; onClick?: () => void }[],
);
// A closed Mantine menu renders no dropdown in static markup; list every item instead.
vi.mock("@mantine/core", async (original) => {
  const actual = await original<typeof import("@mantine/core")>();
  const Menu = Object.assign(
    ({ children }: { children?: ReactNode }) =>
      createElement("div", null, children),
    {
      extend: actual.Menu.extend,
      Target: ({ children }: { children?: ReactNode }) =>
        createElement(Fragment, null, children),
      Dropdown: ({ children }: { children?: ReactNode }) =>
        createElement("div", null, children),
      Label: ({ children }: { children?: ReactNode }) =>
        createElement("span", null, children),
      Item: (props: {
        children?: unknown;
        disabled?: boolean;
        onClick?: () => void;
      }) => {
        menuItems.push(props);
        return createElement(
          "button",
          { type: "button", disabled: props.disabled },
          props.children as ReactNode,
        );
      },
    },
  );
  return { ...actual, Menu };
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
  provider: ProviderKind.Claude,
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
  menuItems.length = 0;
});

it("marks only the exact restored account action for the dialog focus owner", () => {
  const personal = { ...account, accountKey: "personal", label: "Personal" };
  const team = { ...account, accountKey: "team", label: "Team" };
  const byServer = { local: [personal, team] };
  const rows = serverAccountRows([local], byServer);
  const onInitialFocus = vi.fn();
  const { html } = render({
    servers: [local],
    accountsByServer: byServer,
    initialFocusIdentity: required(rows[1]).key,
    onInitialFocus,
  });
  const personalStart = html.indexOf('title="Personal"');
  const teamStart = html.indexOf('title="Team"');
  expect(personalStart).toBeGreaterThan(0);
  expect(teamStart).toBeGreaterThan(personalStart);
  expect(html.slice(personalStart, teamStart)).not.toContain("data-autofocus");
  expect(html.slice(teamStart)).toContain('data-autofocus="true"');
  expect(html.match(/data-autofocus=/g)).toHaveLength(1);
  const target = controls.find(
    (button) => Reflect.get(button, "data-autofocus") === true,
  );
  if (target?.onFocus) Reflect.apply(target.onFocus, null, []);
  expect(onInitialFocus).toHaveBeenCalledOnce();
  for (const button of controls) {
    if (button !== target && button.onFocus)
      Reflect.apply(button.onFocus, null, []);
  }
  expect(onInitialFocus).toHaveBeenCalledOnce();
  expect(render().html).not.toContain("data-autofocus");
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
    accountsByServer: { remote: [{ ...account, status: "signedOut" }] },
    servicesFor: () => undefined,
  });
  expect(html).toContain("Build server: Connection unknown");
  const login = controls.find(
    (button) => button["aria-label"] === "Sign in to Work on Build server",
  );
  expect(login?.disabled).toBe(true);
});

it("shows an exact-account sign-in action without opening account details", () => {
  const { html, startAccount } = render({
    accountsByServer: { local: [{ ...account, status: "signedOut" }] },
  });
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

it("binds each visible duplicate profile label to that profile's exact sign-in key", () => {
  const { startAccount } = render({
    accountsByServer: {
      local: [
        {
          ...account,
          accountKey: "personal",
          label: "Personal",
          plan: "Max",
          status: "signedOut",
        },
        {
          ...account,
          accountKey: "team",
          label: "Team",
          plan: "Team",
          status: "signedOut",
        },
      ],
    },
  });
  for (const [label, accountKey] of [
    ["Personal", "personal"],
    ["Team", "team"],
  ]) {
    const login = controls.find(
      (button) => button["aria-label"] === `Sign in to ${label} on Workstation`,
    );
    expect(login).toBeDefined();
    login?.onClick?.(
      {} as Parameters<NonNullable<ActionButtonProps["onClick"]>>[0],
    );
    expect(startAccount).toHaveBeenLastCalledWith(
      "local",
      expect.objectContaining({ label, accountKey }),
    );
  }
});

it("keeps both ambiguous keyless profiles visible without sending a login", () => {
  const { accountKey: _key, ...legacy } = account;
  const { startAccount } = render({
    accountsByServer: { local: [legacy, { ...legacy }] },
  });
  const logins = controls.filter(
    (button) => button["aria-label"] === "Sign in to Work on Workstation",
  );
  expect(logins).toHaveLength(2);
  for (const login of logins) {
    expect(login.disabled).toBe(true);
    login.onClick?.(
      {} as Parameters<NonNullable<ActionButtonProps["onClick"]>>[0],
    );
  }
  expect(startAccount).not.toHaveBeenCalled();
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

it.each(["signedOut", "changed", "error"])(
  "shows the inline sign-in button for a %s account",
  (status) => {
    const { startAccount } = render({
      accountsByServer: { local: [{ ...account, status }] },
    });
    const login = controls.find(
      (button) => button["aria-label"] === "Sign in to Work on Workstation",
    );
    expect(login).toBeDefined();
    expect(login?.disabled).not.toBe(true);
    login?.onClick?.(
      {} as Parameters<NonNullable<ActionButtonProps["onClick"]>>[0],
    );
    expect(startAccount).toHaveBeenCalledOnce();
  },
);

it("hides the inline sign-in button for a healthy account and keeps sign-in in the account menu", () => {
  const { startAccount } = render();
  expect(
    controls.find(
      (button) => button["aria-label"] === "Sign in to Work on Workstation",
    ),
  ).toBeUndefined();
  const again = menuItems.find(
    (item) => item.children === "Sign in again on Workstation",
  );
  expect(again).toBeDefined();
  expect(again?.disabled).not.toBe(true);
  expect(
    menuItems.some((item) => item.children === "Sign in again on Build server"),
  ).toBe(false);
  again?.onClick?.();
  expect(startAccount).toHaveBeenCalledExactlyOnceWith("local", {
    action: "account-sign-in",
    accountKey: "profile-exact",
    provider: "claude",
    email: account.email,
    label: account.label,
    serverLabel: local.label,
  });
});

it("shows Offline only on the offline peer, not on the live local account", () => {
  const { html } = render({
    accountsByServer: { local: [account], remote: [account] },
  });
  expect(html).toContain("Build server: Offline");
  expect(html).not.toContain("Workstation: Offline");
});

it.each([undefined, "idle", "connecting"])(
  "does not report unknown or %s transport as offline",
  (status) => {
    const { html } = render({
      accountsByServer: { local: [{ ...account, status: "signedOut" }] },
      statuses: status ? { local: status } : {},
    });
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
  const { html } = render({
    accountsByServer: { local: [{ ...account, status: "signedOut" }] },
    statuses: { local: status },
  });
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

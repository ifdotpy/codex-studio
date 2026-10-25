import { beforeEach, expect, it, vi } from "vitest";
import type { Account, AccountsState } from "./Accounts";
import type { ServerCommand } from "../servers/navigation";

const hooks = vi.hoisted(() => ({
  effects: [] as (() => void | (() => void))[],
  writes: [] as unknown[],
}));
vi.mock("react", async (original) => ({
  ...(await original<typeof import("react")>()),
  useEffect: (effect: () => void | (() => void)) => hooks.effects.push(effect),
  useLayoutEffect: () => {},
  useCallback: (callback: unknown) => callback,
  useRef: (current: unknown) => ({ current }),
  useState: (initial: unknown) => [
    typeof initial === "function" ? initial() : initial,
    (value: unknown) => hooks.writes.push(value),
  ],
}));
vi.mock("../servers/ServerServicesContext", async (original) => ({
  ...(await original<typeof import("../servers/ServerServicesContext")>()),
  useServerApi: () => ({ post: vi.fn() }),
  useServerStorage: () => ({ getItem: () => null, setItem: vi.fn() }),
}));
import Accounts from "./Accounts";
const personal: Account = {
  id: "personal",
  provider: "claude",
  email: "same@example.test",
  label: "Personal",
  status: "ready",
  home: "/fixture/personal",
  source: "fixture",
};
const team: Account = {
  ...personal,
  id: "team",
  label: "Team",
  home: "/fixture/team",
};
beforeEach(() => {
  hooks.effects = [];
  hooks.writes = [];
});

async function receive(
  command: Extract<
    ServerCommand,
    { action: "account-action" | "account-sign-in" }
  >,
) {
  const data: AccountsState = {
    accounts: [personal, team],
    archivedAccounts: [],
    defaultAccountKey: "personal",
    defaultAccountGroups: [],
    logins: [],
    supportsDelete: true,
    supportsDisconnect: true,
  };
  Accounts({
    state: {
      data,
      setData: vi.fn(),
      error: "",
      refresh: async () => data,
      scope: "fixture",
    },
    onError: vi.fn(),
    externalCommand: command,
  });
  const stops = hooks.effects.map((effect) => effect());
  for (let step = 0; step < 4; step++) await Promise.resolve();
  for (const stop of stops) stop?.();
}

it("opens the exact Team profile for a configure command, not the first matching email", async () => {
  await receive({
    action: "account-action",
    operation: "configure-profile",
    provider: "claude",
    email: personal.email!,
    label: "Team",
    accountKey: "team",
  });
  expect(hooks.writes).toContain(team);
  expect(hooks.writes).not.toContain(personal);
});

it("does not open Add or a guessed profile for an ambiguous legacy sign-in command", async () => {
  await receive({
    action: "account-sign-in",
    provider: "claude",
    email: personal.email!,
    label: "Personal",
    serverLabel: "Fixture",
  });
  expect(hooks.writes).not.toContain(true);
  expect(hooks.writes).not.toContain(personal);
  expect(hooks.writes).not.toContain(team);
  expect(hooks.writes).toContain(
    "Multiple saved accounts match. Select the exact profile.",
  );
});

it("does not open Add or another profile when an exact sign-in key is missing", async () => {
  await receive({
    action: "account-sign-in",
    provider: "claude",
    email: personal.email!,
    label: "Personal",
    serverLabel: "Fixture",
    accountKey: "deleted",
  });
  expect(hooks.writes).not.toContain(true);
  expect(hooks.writes).not.toContain(personal);
  expect(hooks.writes).not.toContain(team);
});

it("preserves an explicit Add request even when the email already has profiles", async () => {
  await receive({
    action: "account-sign-in",
    provider: "claude",
    email: personal.email!,
    label: "Another",
    serverLabel: "Fixture",
    forceAdd: true,
  });
  expect(hooks.writes).toContain(true);
  expect(hooks.writes).not.toContain(personal);
  expect(hooks.writes).not.toContain(team);
});

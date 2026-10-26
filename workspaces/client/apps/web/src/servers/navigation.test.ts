import { snapshotFixture } from "../snapshotFixture";
import assert from "node:assert/strict";
import { navigationSnapshot } from "./navigation.ts";
import type { Agent } from "../types";
import { expect, it } from "vitest";
import { isServerCommand, readServerAccounts } from "./navigation";

it("preserves exact account keys in the received public account snapshot", () => {
  const account = {
    provider: "claude",
    email: "work@example.test",
    plan: "Max",
    status: "ready",
    label: "Work",
    isDefault: true,
  };
  expect(readServerAccounts([{ ...account, accountKey: "exact" }])).toEqual([
    { ...account, accountKey: "exact" },
  ]);
  expect(readServerAccounts([account])).toEqual([account]);
  expect(readServerAccounts([{ ...account, accountKey: 12 }])).toBeNull();
  expect(readServerAccounts([{ ...account, accountKey: "" }])).toBeNull();
});

it("accepts an exact public account key and rejects malformed keys", () => {
  const command = {
    action: "account-sign-in",
    provider: "claude",
    email: null,
    label: "Work",
    serverLabel: "Remote",
    accountKey: "profile-exact",
  };
  expect(isServerCommand(command)).toBe(true);
  expect(isServerCommand({ ...command, accountKey: 12 })).toBe(false);
  expect(isServerCommand({ ...command, accountKey: "" })).toBe(false);
  expect(isServerCommand({ ...command, forceAdd: true })).toBe(false);
});

it.each(["configure-profile", "rename", "default", "disconnect", "remove"])(
  "validates exact account keys for %s commands",
  (operation) => {
    const command = {
      action: "account-action",
      operation,
      provider: "claude",
      email: "same@example.test",
      label: "Team",
      accountKey: "team",
    };
    expect(isServerCommand(command)).toBe(true);
    expect(isServerCommand({ ...command, accountKey: 12 })).toBe(false);
    expect(isServerCommand({ ...command, accountKey: "" })).toBe(false);
  },
);

it("accepts account commands with only public account identity fields", () => {
  expect(
    isServerCommand({
      action: "account-sign-in",
      provider: "claude",
      email: "user@example.com",
      label: "Work",
      serverLabel: "Remote",
      forceAdd: true,
    }),
  ).toBe(true);
  expect(
    isServerCommand({
      action: "account-action",
      operation: "rename",
      provider: "codex",
      email: null,
      label: "Personal",
      nextLabel: "Home",
    }),
  ).toBe(true);
});

it("rejects malformed account providers and operations", () => {
  expect(
    isServerCommand({
      action: "account-sign-in",
      provider: "gemini",
      email: "user@example.com",
      label: "Work",
      serverLabel: "Remote",
    }),
  ).toBe(false);
  expect(
    isServerCommand({
      action: "account-action",
      operation: "login",
      provider: "claude",
      email: null,
      label: "Work",
    }),
  ).toBe(false);
});

it("shows the active target chat and hides the source move record", () => {
  const chat: Agent = {
    id: "same-id",
    source: "managed",
    isLead: true,
    cwd: "/project",
  };
  const source = navigationSnapshot(
    snapshotFixture([{ ...chat, movedTo: { server: "target", move: "move" } }]),
    null,
    "",
  );
  const target = navigationSnapshot(
    snapshotFixture([
      { ...chat, movedFrom: { server: "source", move: "move" } },
    ]),
    null,
    "",
  );
  assert.deepEqual(source.chats, []);
  assert.deepEqual(
    navigationSnapshot(
      snapshotFixture([{ ...chat, moveImportPending: true }]),
      null,
      "",
    ).chats,
    [],
  );
  assert.deepEqual(
    target.chats.map((row) => row.id),
    ["same-id"],
  );
});

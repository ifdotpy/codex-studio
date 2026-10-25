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

it("rejects malformed account commands and credential-shaped fields", () => {
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

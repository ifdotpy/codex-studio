import { expect, it } from "vitest";
import { isServerCommand } from "./navigation";

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

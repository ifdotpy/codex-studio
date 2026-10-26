import { expect, test } from "vitest";
import {
  defaultWorkspaceMode,
  isNewChatSettings,
  settingsForNewChatAccount,
} from "./newChatSettings";
import { isServerCommand } from "../servers/navigation";

test.each([
  ["Darwin", "layr"],
  ["Linux", "worktree"],
  ["Windows", "worktree"],
  [undefined, "worktree"],
])("%s defaults to %s", (system, mode) =>
  expect(defaultWorkspaceMode(system)).toBe(mode),
);
test("accepts the actual selector values and rejects receipt overrides", () => {
  const settings = {
    workspaceMode: "layr",
    model: "gpt-6-astra",
    effort: "high",
    fast_mode: true,
    worker_defaults: {
      model: null,
      effort: null,
      fast_mode: false,
      account_key: null,
    },
    review_defaults: { model: null, effort: null },
  };
  expect(isNewChatSettings(settings)).toBe(true);
  expect(isServerCommand({ action: "new-chat", settings })).toBe(true);
  for (const patch of [
    { id: "other" },
    { previous: "other" },
    { workspaceMode: "shared" },
    { fast_mode: "true" },
    { account_key: null },
    { worker_defaults: [] },
    {
      worker_defaults: {
        model: "x",
        effort: null,
        fast_mode: false,
        id: "other",
      },
    },
    { review_defaults: { model: null } },
  ]) {
    expect(isNewChatSettings({ ...settings, ...patch })).toBe(false);
    expect(
      isServerCommand({
        action: "new-chat",
        settings: { ...settings, ...patch },
      }),
    ).toBe(false);
  }
});

test("switching the lead account resets account-relative model choices", () => {
  const changed = settingsForNewChatAccount(
    {
      account_key: "codex",
      model: "gpt-6-sol",
      effort: "high",
      fast_mode: true,
      worker_defaults: {
        model: "gpt-6-luna",
        effort: "high",
        fast_mode: false,
        account_key: null,
      },
      review_defaults: { model: "gpt-6-sol", effort: "high" },
    },
    "claude",
  );

  expect(changed).toEqual({
    account_key: "claude",
    model: undefined,
    effort: null,
    fast_mode: false,
    daybreak_enabled: false,
    worker_defaults: undefined,
    review_defaults: undefined,
  });
});

test("switching the lead account preserves explicitly account-pinned workers", () => {
  const workerDefaults = {
    model: "gpt-6-luna",
    effort: "high",
    fast_mode: false,
    account_key: "worker-codex",
  };
  const changed = settingsForNewChatAccount(
    {
      model: "claude-sonnet",
      worker_defaults: workerDefaults,
      review_defaults: { model: "gpt-6-sol", effort: "high" },
    },
    "claude",
  );

  expect(changed.account_key).toBe("claude");
  expect(changed.model).toBeUndefined();
  expect(changed.worker_defaults).toBe(workerDefaults);
  expect(changed.review_defaults).toBeUndefined();
});

test("switching accounts resolves a pinned worker's inherited model first", () => {
  const changed = settingsForNewChatAccount(
    {
      model: "gpt-6-sol",
      worker_defaults: {
        model: null,
        effort: "high",
        fast_mode: false,
        account_key: "worker-codex",
      },
    },
    "claude",
    "gpt-6-sol",
  );

  expect(changed.worker_defaults).toEqual({
    model: "gpt-6-sol",
    effort: null,
    fast_mode: false,
    account_key: "worker-codex",
  });
});

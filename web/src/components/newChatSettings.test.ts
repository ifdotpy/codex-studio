import { expect, test } from "vitest";
import { defaultWorkspaceMode, isNewChatSettings } from "./newChatSettings";
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

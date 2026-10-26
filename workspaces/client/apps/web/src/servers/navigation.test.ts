import { expect, it } from "vitest";
import assert from "node:assert/strict";
import { navigationSnapshot, isServerCommand } from "./navigation";

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

// These upstream fixtures omit snapshot metadata, thread timestamps and move IDs.
const partialSnapshot = (data: object) =>
  data as NonNullable<Parameters<typeof navigationSnapshot>[0]>;

it("shows the active target chat and hides the source move record", () => {
  const chat = {
    id: "same-id",
    source: "managed",
    isLead: true,
    cwd: "/project",
  };
  const source = navigationSnapshot(
    partialSnapshot({ threads: [{ ...chat, movedTo: { server: "target" } }] }),
    null,
    "",
  );
  const target = navigationSnapshot(
    partialSnapshot({
      threads: [{ ...chat, movedFrom: { server: "source" } }],
    }),
    null,
    "",
  );
  assert.deepEqual(source.chats, []);
  assert.deepEqual(
    navigationSnapshot(
      partialSnapshot({ threads: [{ ...chat, moveImportPending: true }] }),
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

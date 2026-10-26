import { expect, test } from "vitest";
import {
  layrSyncSummary,
  layrSyncText,
  macSyncOpen,
  projectChip,
  type LayrSync,
} from "./MacSync";

const sync = (values: Partial<LayrSync>): LayrSync => ({
  projectId: "p1",
  state: "synced",
  enabled: true,
  conflicts: [],
  choices: [],
  withAgent: [],
  deletes: 0,
  reason: null,
  ...values,
});

test.each([
  [sync({}), "Folder synced with the VM"],
  [sync({ state: "syncing" }), "Folder sync is running"],
  [sync({ state: "waiting" }), "Folder sync waits for the VM"],
  [
    sync({ state: "waiting", withAgent: ["a.txt"] }),
    "Folder sync: the agent combines 1 file",
  ],
  [
    sync({ state: "conflict", conflicts: ["a", "b"], choices: ["b"] }),
    "Folder sync: 2 files changed on both sides",
  ],
  [sync({ state: "paused", enabled: false }), "Folder sync paused"],
  [
    sync({ state: "paused", deletes: 600 }),
    "Folder sync paused: confirm 600 files deleted locally",
  ],
  [
    sync({ state: "error", reason: "The VM stopped" }),
    "Folder sync failed: The VM stopped",
  ],
] as const)("states %j in plain words", (value, text) => {
  expect(layrSyncText(value)).toBe(text);
  expect(layrSyncSummary([{ sync: value }])?.title).toBe(text);
});

test("the LAYR chip marks a sync that needs the user and hides a quiet one", () => {
  const layr = { state: "ready" as const, title: "Ready for layr chats" };
  const chip = (value: LayrSync) =>
    projectChip(layr, layrSyncSummary([{ sync: value }]));
  for (const quiet of [
    sync({}),
    sync({ state: "syncing" }),
    sync({ state: "paused", enabled: false }),
  ])
    expect(chip(quiet)).toEqual({ summary: layr, attention: false });
  const conflict = chip(
    sync({ state: "conflict", conflicts: ["a"], choices: ["a", "b"] }),
  );
  expect(macSyncOpen(sync({ conflicts: ["a"], choices: ["a", "b"] }))).toEqual([
    "a",
    "b",
  ]);
  expect(conflict).toEqual({
    summary: {
      state: "ready",
      title: "Ready for layr chats\nFolder sync: 2 files changed on both sides",
    },
    attention: true,
  });
  expect(chip(sync({ state: "paused", deletes: 3 }))?.attention).toBe(true);
  expect(
    chip(sync({ state: "error", reason: "The VM stopped" }))?.attention,
  ).toBe(true);
  expect(projectChip(null, layrSyncSummary([{ sync: sync({}) }]))).toBeNull();
});

test("the summary shows the server that needs attention first", () => {
  const summary = layrSyncSummary([
    { server: "MAC", sync: sync({}) },
    { server: "MINI", sync: sync({ state: "error", reason: "VM stopped" }) },
  ]);
  expect(summary?.state).toBe("error");
  expect(summary?.title).toBe(
    "MAC: Folder synced with the VM\nMINI: Folder sync failed: VM stopped",
  );
  expect(layrSyncSummary([])).toBeNull();
});

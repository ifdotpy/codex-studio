import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { expect, test } from "vitest";
import ProjectLayrSync, {
  describeWithLayrSync,
  layrSyncSummary,
  layrSyncText,
  macSyncOpen,
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
  [sync({}), "Mac folder synced with the VM"],
  [sync({ state: "syncing" }), "Mac folder sync is running"],
  [sync({ state: "waiting" }), "Mac folder sync waits for the VM"],
  [
    sync({ state: "waiting", withAgent: ["a.txt"] }),
    "Mac folder sync: the agent combines 1 file",
  ],
  [
    sync({ state: "conflict", conflicts: ["a", "b"], choices: ["b"] }),
    "Mac folder sync: 2 files changed on both sides",
  ],
  [sync({ state: "paused", enabled: false }), "Mac folder sync paused"],
  [
    sync({ state: "paused", deletes: 600 }),
    "Mac folder sync paused: confirm 600 files deleted on the Mac",
  ],
  [
    sync({ state: "error", reason: "The VM stopped" }),
    "Mac folder sync failed: The VM stopped",
  ],
] as const)("states %j in plain words", (value, text) => {
  expect(layrSyncText(value)).toBe(text);
  const markup = renderToStaticMarkup(
    createElement(ProjectLayrSync, {
      summary: layrSyncSummary([{ sync: value }]),
    }),
  );
  expect(markup).toContain(`data-state="${value.state}"`);
  expect(markup).toContain(`title="${text}"`);
  expect(markup).toContain('aria-hidden="true"');
});

test("the chip counts the files that wait for a choice", () => {
  const value = sync({
    state: "conflict",
    conflicts: ["a"],
    choices: ["a", "b"],
  });
  expect(macSyncOpen(value)).toEqual(["a", "b"]);
  const markup = renderToStaticMarkup(
    createElement(ProjectLayrSync, {
      summary: layrSyncSummary([{ sync: value }]),
    }),
  );
  expect(markup).toMatch(
    /^<small class="project-layr-sync"[^>]*>SYNC 2<\/small>$/,
  );
});

test("the row shows the server that needs attention first", () => {
  const summary = layrSyncSummary([
    { server: "MAC", sync: sync({}) },
    { server: "MINI", sync: sync({ state: "error", reason: "VM stopped" }) },
  ]);
  expect(summary?.state).toBe("error");
  expect(summary?.title).toBe(
    "MAC: Mac folder synced with the VM\nMINI: Mac folder sync failed: VM stopped",
  );
  expect(describeWithLayrSync(summary, "Drag.")).toBe(
    "MAC: Mac folder synced with the VM; MINI: Mac folder sync failed: VM stopped. Drag.",
  );
  expect(layrSyncSummary([])).toBeNull();
  expect(describeWithLayrSync(null, "Drag.")).toBe("Drag.");
});

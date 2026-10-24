import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { expect, test, vi } from "vitest";
import type { Agent } from "../../types";
vi.mock("../../servers/ServerServicesContext", () => ({
  useServerApi: () => ({ get: vi.fn() }),
  useServerResourceReads: () => vi.fn(),
  useServerFileDialogs: () => ({
    canRevealPath: () => true,
    revealPath: vi.fn(),
  }),
}));
import { RetainedWorktree, TeamRetainedWorktrees } from "./RetainedWorktrees";
function render(component: ReturnType<typeof createElement>) {
  return renderToStaticMarkup(createElement(MantineProvider, null, component));
}
const worker: Agent = {
  id: "worker",
  worktree: true,
  rootId: "lead",
  status: "completed",
};

test("retained worktrees show reason and path without a delete action", () => {
  const html = render(
    createElement(RetainedWorktree, {
      agent: {
        ...worker,
        workspaceCleanupIntent: {
          phase: "not_sent",
          reason: "Submodule vendor has changes",
          path: "/work/retained",
        },
      },
    }),
  );
  for (const value of [
    "Worktree retained",
    "Submodule vendor has changes",
    "/work/retained",
    "Open folder",
  ])
    expect(html).toContain(value);
  expect(html).not.toContain("Delete");
  expect(
    render(createElement(RetainedWorktree, { agent: worker })),
  ).not.toContain("Worktree retained");
});

test("the team lists the retained folders of deleted workers by name", () => {
  const html = render(
    createElement(TeamRetainedWorktrees, {
      items: [
        {
          id: "deleted",
          name: "Deleted worker",
          cleanup: { phase: "unknown", reason: null, path: "/work/kept" },
        },
      ],
    }),
  );
  for (const value of [
    "Deleted worker",
    "Cleanup outcome unconfirmed",
    "/work/kept",
  ])
    expect(html).toContain(value);
  expect(html).not.toMatch(/budget|Turns used|capabilities/i);
});

import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { expect, test } from "vitest";
import type { Agent } from "../../types";
import WorkspaceBadge, { workspaceBadgeInfo } from "./WorkspaceBadge";

const agent = (values: Partial<Agent>): Agent => ({
  id: "worker",
  isLead: false,
  cwd: "/projects/example",
  ...values,
});

test.each([
  ["layr-native", "Layr", "Native Layr workspace"],
  ["worktree", "WT", "Git worktree"],
  ["shared", "SHARED", "Shared folder"],
] as const)("shows %s workspace as %s", (workspaceMode, label, name) => {
  const current = agent({ workspaceMode });
  const info = workspaceBadgeInfo(current);
  expect(info).toEqual({ label, title: `${name} · /projects/example` });
  const markup = renderToStaticMarkup(
    createElement(WorkspaceBadge, { agent: current }),
  );
  expect(markup).toContain(`title="${name} · /projects/example"`);
  expect(markup).toContain(`>${label}</span>`);
});

test("hides the badge for leads and unknown remote workspaces", () => {
  expect(workspaceBadgeInfo(agent({ isLead: true }))).toBeNull();
  expect(
    workspaceBadgeInfo(
      agent({ remoteWorker: { server: "remote", link: "link" } }),
    ),
  ).toBeNull();
});

test.each([
  ["layr-native", "Layr", "Native Layr workspace"],
  ["worktree", "WT", "Git worktree"],
] as const)(
  "shows remote %s mode even when local workspace flags are false",
  (workspaceMode, label, name) => {
    expect(
      workspaceBadgeInfo(
        agent({
          cwd: "/remote/repo",
          remoteWorker: { server: "remote", link: "link" },
          workspaceMode,
          worktree: false,
        }),
      ),
    ).toEqual({ label, title: `${name} · /remote/repo` });
  },
);

test.each([
  ["layr", "Layr VM"],
  ["layr-native", "Layr"],
  ["worktree", "WT"],
] as const)("shows the chat workspace %s as %s", (workspaceMode, label) => {
  const current = agent({ isLead: true, workspaceMode });
  const markup = renderToStaticMarkup(
    createElement(WorkspaceBadge, { agent: current }),
  );
  expect(workspaceBadgeInfo(current)?.label).toBe(label);
  expect(markup).toContain(`>${label}</span>`);
});

test.each([false, true])("shows the layr worker badge, remote=%s", (remote) => {
  const current = agent({
    ...(remote ? { remoteWorker: { server: "remote", link: "link" } } : {}),
    workspaceMode: "layr",
  });
  expect(workspaceBadgeInfo(current)).toEqual({
    label: "Layr VM",
    title: "Virtual machine with Layr · /projects/example",
  });
});

test("missing modes retain lead and remote badge visibility", () => {
  expect(workspaceBadgeInfo()).toBeNull();
  for (const workspaceMode of [undefined, null]) {
    expect(workspaceBadgeInfo(agent({ workspaceMode }))).toBeNull();
    expect(
      workspaceBadgeInfo(
        agent({ isLead: true, workspaceMode, worktree: true }),
      ),
    ).toBeNull();
    expect(
      workspaceBadgeInfo(
        agent({
          workspaceMode,
          remoteWorker: { server: "remote", link: "link" },
        }),
      ),
    ).toBeNull();
  }
  expect(
    workspaceBadgeInfo(agent({ isLead: true, workspaceMode: "shared" })),
  ).toBeNull();
  expect(
    workspaceBadgeInfo(
      agent({
        cwd: null,
        workspaceMode: "worktree",
        remoteWorker: { server: "remote", link: "link" },
      }),
    ),
  ).toBeNull();
});

test.each([
  [{ worktree: true, role: "reviewer" }, "WT"],
  [{ role: "reviewer" }, "SHARED"],
  [{ worktree: false }, "SHARED"],
  [{ worktree: "/legacy/worktree" }, undefined],
] satisfies [Partial<Agent>, string | undefined][])(
  "preserves legacy workspace inference for %j",
  (values, label) =>
    expect(workspaceBadgeInfo(agent(values))?.label).toBe(label),
);

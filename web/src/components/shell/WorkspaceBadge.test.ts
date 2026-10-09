import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { expect, test } from "vitest";
import type { Agent } from "../../types";
import WorkspaceBadge, { workspaceBadgeInfo } from "./WorkspaceBadge";

const agent = (values: Partial<Agent>) =>
  ({
    id: "worker",
    isLead: false,
    cwd: "/projects/example",
    ...values,
  }) as Agent;

test.each([
  ["image", "host", "ASIF", "Apple Sparse Image Format workspace"],
  ["image", "linux", "VM", "Linux virtual machine workspace"],
  ["worktree", "host", "WT", "Git worktree"],
  ["shared", "host", "SHARED", "Shared folder"],
] as const)(
  "shows %s workspace as %s",
  (workspaceMode, environment, label, name) => {
    const current = agent({ workspaceMode, environment });
    const info = workspaceBadgeInfo(current);
    expect(info).toEqual({ label, title: `${name} · /projects/example` });
    const markup = renderToStaticMarkup(
      createElement(WorkspaceBadge, { agent: current }),
    );
    expect(markup).toContain(`title="${name} · /projects/example"`);
    expect(markup).toContain(`>${label}</span>`);
  },
);

test("uses the Linux overlay badge when the host environment uses an overlay", () => {
  expect(
    workspaceBadgeInfo(
      agent({
        workspaceMode: "image",
        environment: "host",
        workspaceBackend: "vm",
      }),
    ),
  ).toEqual({
    label: "VM",
    title: "Linux virtual machine workspace · /projects/example",
  });
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
  ["image", "ASIF", "Apple Sparse Image Format workspace"],
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
          workspaceBackend: workspaceMode === "image" ? "asif" : null,
          imageWorkspace: false,
          worktree: false,
        }),
      ),
    ).toEqual({ label, title: `${name} · /remote/repo` });
  },
);

test.each([
  ["layr", "LAYR"],
  ["image", "ASIF"],
  ["worktree", "WT"],
])("shows the chat workspace %s as %s", (mode, label) => {
  const current = { ...agent({ isLead: true }), workspaceMode: mode } as Agent;
  const markup = renderToStaticMarkup(
    createElement(WorkspaceBadge, { agent: current }),
  );
  expect(workspaceBadgeInfo(current)?.label).toBe(label);
  expect(markup).toContain(`>${label}</span>`);
});

import { Button, MantineProvider } from "@mantine/core";
import {
  Children,
  createElement,
  isValidElement,
  type ReactElement,
  type ReactNode,
} from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import { theme } from "../theme";
import type { Agent } from "../types";
import ProjectFolderDialog, {
  FinderLocation,
  VM_FOLDER_NOTE,
  finderLocation,
  type FinderView,
} from "./ProjectFolderDialog";

const VM_CWD = "/var/lib/codex-studio/layr/projects/app/lines/main";
const STUDIO_PATH = "/Users/me/Studio/app";

const agent = (values: Partial<Agent>) =>
  ({ id: "chat", isLead: true, cwd: "/Users/me/app", ...values }) as Agent;

const render = (element: ReactElement) =>
  renderToStaticMarkup(createElement(MantineProvider, { theme }, element));

function buttons(node: ReactNode): ReactElement<{ onClick: () => void }>[] {
  if (!isValidElement<{ children?: ReactNode }>(node)) return [];
  if (node.type === Button)
    return [node as ReactElement<{ onClick: () => void }>];
  return Children.toArray(node.props.children).flatMap(buttons);
}

describe("Finder location", () => {
  it("shows the Studio folder only when the VM view is mounted", () => {
    expect(
      finderLocation({
        kind: "vm",
        state: "mounted",
        path: STUDIO_PATH,
        error: null,
      }),
    ).toEqual({ path: STUDIO_PATH });
    expect(
      finderLocation({
        kind: "vm",
        state: "failed",
        path: null,
        error: "The authenticated SMB mount failed: denied",
      }),
    ).toEqual({ reason: "The authenticated SMB mount failed: denied" });
    expect(
      finderLocation({
        kind: "vm",
        state: "unmounted",
        path: null,
        error: null,
      }),
    ).toEqual({ reason: "Not mounted yet" });
    expect(
      finderLocation({ kind: "vm", state: "mounted", path: null, error: null }),
    ).toEqual({ reason: "Not mounted yet" });
  });

  it("reveals the mounted Studio folder, not the VM path", () => {
    const onReveal = vi.fn();
    const view: FinderView = {
      kind: "vm",
      state: "mounted",
      path: STUDIO_PATH,
      error: null,
    };
    const [button] = buttons(
      FinderLocation({ view, canReveal: true, onReveal }),
    );
    button!.props.onClick();
    expect(onReveal).toHaveBeenCalledWith(STUDIO_PATH);

    const markup = render(
      createElement(FinderLocation, { view, canReveal: true, onReveal }),
    );
    expect(markup).toContain(STUDIO_PATH);
    expect(markup).toContain("Show in Finder");
  });

  it("has no button outside the desktop app or without a mount", () => {
    const onReveal = vi.fn();
    const mounted = render(
      createElement(FinderLocation, {
        view: { kind: "vm", state: "mounted", path: STUDIO_PATH, error: null },
        canReveal: false,
        onReveal,
      }),
    );
    expect(mounted).toContain(STUDIO_PATH);
    expect(mounted).not.toContain("Show in Finder");

    const failed = render(
      createElement(FinderLocation, {
        view: { kind: "vm", state: "failed", path: null, error: "denied" },
        canReveal: true,
        onReveal,
      }),
    );
    expect(failed).toContain("<p>denied</p>");
    expect(failed).not.toContain("Show in Finder");
    expect(
      buttons(
        FinderLocation({
          view: { kind: "vm", state: "unmounted", path: null, error: null },
          canReveal: true,
          onReveal,
        }),
      ),
    ).toEqual([]);
  });
});

describe("Project folder dialog", () => {
  it("keeps the native folder and its Finder button", () => {
    const onReveal = vi.fn();
    const native = agent({ executionMode: "native" });
    const markup = render(
      createElement(ProjectFolderDialog, {
        agent: native,
        canReveal: true,
        onReveal,
      }),
    );
    expect(markup).toContain("<p>/Users/me/app</p>");
    expect(markup).toContain("To use another folder, start a new chat.");
    expect(markup).toContain("Show in Finder");
    expect(markup).not.toContain(VM_FOLDER_NOTE);
    const [button] = buttons(
      ProjectFolderDialog({ agent: native, canReveal: true, onReveal }),
    );
    button!.props.onClick();
    expect(onReveal).toHaveBeenCalledWith("/Users/me/app");

    expect(
      render(
        createElement(ProjectFolderDialog, {
          agent: agent({}),
          canReveal: false,
          onReveal,
        }),
      ),
    ).not.toContain("Show in Finder");
  });

  it("explains the VM view instead of the VM path while it loads", () => {
    const markup = render(
      createElement(ProjectFolderDialog, {
        agent: agent({
          executionMode: "vm",
          layrProjectId: "app",
          cwd: VM_CWD,
        }),
        canReveal: true,
        onReveal: vi.fn(),
      }),
    );
    expect(markup).toContain(VM_FOLDER_NOTE.replace("'", "&#x27;"));
    expect(markup).not.toContain(VM_CWD);
    expect(markup).not.toContain("Show in Finder");
  });
});

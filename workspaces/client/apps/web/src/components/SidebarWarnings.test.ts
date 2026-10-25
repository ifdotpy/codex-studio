import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { expect, test, vi } from "vitest";
import type { Agent, Snapshot } from "../types";
const read = vi.hoisted(() => vi.fn(() => ({ items: [] })));
vi.mock("../hooks", () => ({ useMessages: read }));
import SidebarWarnings from "./SidebarWarnings";

test("imported agents read the agent transcript that preserves native warnings", () => {
  const agent = { id: "imported", source: "app-server" } as Agent;
  renderToStaticMarkup(
    createElement(SidebarWarnings, {
      data: {
        stateDir: "/fixture",
        runtime: { nativeNotices: [] },
      } as unknown as Snapshot,
      agent,
      opened: agent.id,
      accountKey: "default",
      syncWorkspaceId: "fixture",
      open: () => {},
    }),
  );
  expect(read).toHaveBeenCalledWith(
    "imported",
    "agent",
    false,
    "/fixture",
    "fixture",
    true,
  );
});

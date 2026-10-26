import { createElement, type ReactNode } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { afterEach, expect, it, vi } from "vitest";
import { snapshotFixture } from "../snapshotFixture";
import Sidebar from "./Sidebar";

// Static markup has no layout, so the measured frame would start as a closed
// drawer. The frame's geometry has its own tests; render its content inline here.
vi.mock("./sidebar/SidebarFrame", () => ({
  SidebarFrame: (p: { children: ReactNode }) =>
    createElement("aside", { id: "sidebar" }, p.children),
  restoreSidebarFocus: () => {},
}));

afterEach(() => vi.unstubAllGlobals());

it("leaves the desktop close action to the workspace header", () => {
  vi.stubGlobal("location", new URL("http://studio.test/"));
  vi.stubGlobal("localStorage", { getItem: () => null });
  vi.stubGlobal("window", { matchMedia: () => ({ matches: false }) });
  const markup = renderToStaticMarkup(
    createElement(
      MantineProvider,
      null,
      createElement(Sidebar, {
        data: snapshotFixture(),
        opened: "",
        indicators: new Map(),
        markingRead: new Set<string>(),
        creating: false,
        mobile: false,
        close: () => {},
        onSearch: () => {},
        open: () => {},
        newChat: () => {},
        addProject: () => {},
        changeProject: () => {},
        projectAccount: () => {},
        rename: async () => {},
        remove: () => {},
        markUnread: () => {},
        refresh: async () => {},
      }),
    ),
  );
  expect(markup).toContain('aria-label="Search chats"');
  expect(markup).not.toContain('aria-label="Close conversations"');
});

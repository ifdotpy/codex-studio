import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { afterEach, expect, it, vi } from "vitest";
import { snapshotFixture } from "../snapshotFixture";
import Sidebar from "./Sidebar";

afterEach(() => vi.unstubAllGlobals());

it("leaves the desktop close action to the workspace header", () => {
  vi.stubGlobal("location", new URL("http://studio.test/"));
  vi.stubGlobal("localStorage", { getItem: () => null });
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
  expect(markup).toContain('aria-label="Conversations"');
  expect(markup).not.toContain('aria-label="Close conversations"');
});

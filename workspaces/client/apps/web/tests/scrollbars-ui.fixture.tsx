import React from "react";
import { createRoot } from "react-dom/client";
import { createPortal } from "react-dom";
import { MantineProvider, ScrollArea } from "@mantine/core";
import { theme } from "../src/theme";
import "@mantine/core/styles.css";
import "../src/style.css";
import "../src/components/sidebar-projects.css";
const content = (
  <div style={{ height: 1200, width: 800 }}>Scrollable content</div>
);
const mount = document.getElementById("fixture");
if (!mount) throw new Error("Missing scrollbar fixture mount");
createRoot(mount).render(
  <MantineProvider theme={theme}>
    <div
      id="messages"
      tabIndex={0}
      style={{ height: 180, width: 260, flex: "none", padding: 8 }}
    >
      {content}
    </div>
    <div
      className="chat-scroll"
      tabIndex={0}
      style={{ height: 180, width: 260, flex: "none" }}
    >
      {content}
    </div>
    <ScrollArea type="always" style={{ height: 180, width: 260 }}>
      {content}
    </ScrollArea>
    {createPortal(
      <textarea
        aria-label="Portal draft"
        defaultValue={"Draft line\n".repeat(100)}
        style={{ height: 180, width: 260 }}
      />,
      document.body,
    )}
  </MantineProvider>,
);

function computedStyles() {
  const properties = [
    "scrollbarColor",
    "scrollbarWidth",
    "backgroundColor",
    "width",
    "borderRadius",
    "overflowY",
    "overflowX",
  ] as const;
  return Object.fromEntries(
    [
      "#messages",
      ".chat-scroll",
      "textarea",
      ".mantine-ScrollArea-viewport",
      '.mantine-ScrollArea-scrollbar[data-orientation="vertical"]',
      '.mantine-ScrollArea-scrollbar[data-orientation="vertical"] > .mantine-ScrollArea-thumb',
    ].map((selector) => {
      const element = document.querySelector(selector);
      if (!element)
        throw new Error("Missing scrollbar fixture element: " + selector);
      const style = getComputedStyle(element);
      return [
        selector,
        Object.fromEntries(properties.map((key) => [key, style[key]])),
      ];
    }),
  );
}

declare global {
  interface Window {
    scrollbarFixture: { computedStyles: typeof computedStyles };
  }
}
window.scrollbarFixture = { computedStyles };

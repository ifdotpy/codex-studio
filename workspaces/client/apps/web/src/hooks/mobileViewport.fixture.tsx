const fixtureRoot = document.getElementById("root");
if (!fixtureRoot) throw new Error("Missing fixture root");

import React from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider, Drawer, Modal } from "@mantine/core";
import "@mantine/core/styles.css";
import "/src/style.css";
import "/src/hooks/viewport-surface.css";
import { useMobileViewport } from "/src/hooks/mobileViewport.ts";
function ViewportOwner({ active }: { active: boolean }) {
  useMobileViewport(active, false);
  return null;
}
function Surface() {
  const [owner, setOwner] = React.useState(1);
  window.selectViewportOwner = setOwner;
  const [portal, setPortal] = React.useState("");
  window.openPortal = setPortal;
  return React.createElement(
    MantineProvider,
    null,
    React.createElement(ViewportOwner, { active: owner === 1 }),
    React.createElement(ViewportOwner, { active: owner === 2 }),
    React.createElement(
      "main",
      {
        id: "viewport-content",
        style: {
          flex: 1,
          minWidth: 0,
          minHeight: 0,
          display: "flex",
          flexDirection: "column",
        },
      },
      React.createElement("header", null, "Header"),
      React.createElement("div", { style: { flex: 1 } }, "Conversation"),
      React.createElement("footer", null, "Composer"),
    ),
    React.createElement(
      Drawer,
      {
        opened: portal === "drawer",
        onClose: () => setPortal(""),
        position: "right",
        size: "100%",
        title: "Drawer",
      },
      React.createElement("textarea", { "aria-label": "Portal note" }),
    ),
    React.createElement(
      Modal,
      {
        opened: portal === "modal",
        onClose: () => setPortal(""),
        fullScreen: true,
        title: "Modal",
      },
      React.createElement("textarea", { "aria-label": "Portal note" }),
    ),
  );
}
const root = createRoot(fixtureRoot);
root.render(React.createElement(Surface));
window.unmountSurface = () => root.unmount();

declare global {
  interface Window {
    selectViewportOwner: (owner: number) => void;
    openPortal: (portal: string) => void;
    unmountSurface: () => void;
    viewportObserverCount: () => number;
    resizeViewport: (
      patch: Partial<Pick<VisualViewport, "height" | "offsetTop" | "scale">>,
    ) => void;
  }
}

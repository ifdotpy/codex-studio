const fixtureRoot = document.getElementById("root");
if (!fixtureRoot) throw new Error("Missing fixture root");

import React, { useEffect, useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider, Modal } from "@mantine/core";
import { WorkspacePanelHost } from "/src/components/shell/WorkspacePanelHost.tsx";
import "@mantine/core/styles.css";
function Form() {
  const [draft, setDraft] = useState("");
  const [formOpened, setFormOpened] = useState(false);
  window.setFormOpened = setFormOpened;
  useEffect(() => {
    window.mounts = (window.mounts || 0) + 1;
  }, []);
  return React.createElement(
    React.Fragment,
    null,
    React.createElement(
      Modal,
      {
        opened: formOpened,
        onClose: () => setFormOpened(false),
        title: "Rule form",
        closeButtonProps: { "aria-label": "Close rule" },
      },
      React.createElement("input", { "aria-label": "Rule draft" }),
    ),
    React.createElement(
      "div",
      { className: "test-scroll", style: { height: 200, overflow: "auto" } },
      React.createElement("textarea", {
        "aria-label": "Draft",
        value: draft,
        onChange: (e: React.ChangeEvent<HTMLTextAreaElement>) =>
          setDraft(e.target.value),
      }),
      React.createElement("div", { style: { height: 1000 } }, "Content"),
    ),
  );
}
function App() {
  const [mode, setMode] = useState<"inline" | "drawer">("inline");
  const [opened, setOpened] = useState(true);
  const [target, setTarget] = useState<HTMLElement | null>(null);
  const [width, setWidth] = useState(540);
  window.changeMode = setMode;
  window.setOpened = setOpened;
  return React.createElement(
    React.Fragment,
    null,
    React.createElement("button", { id: "outside" }, "Conversation"),
    React.createElement("div", {
      ref: setTarget,
      id: "inline",
      style: { height: 650, display: "flex" },
    }),
    React.createElement(WorkspacePanelHost, {
      opened,
      onClose: () => setOpened(false),
      inlineTarget: target,
      panelLayout: {
        mode,
        width,
        minWidth: 360,
        maxWidth: 600,
        rootFontSize: 16,
      },
      onPanelWidthChange: (rem) => {
        window.savedWidth = rem * 16;
        setWidth(rem * 16);
      },
      title: "Workspace",
      drawerSize: "min(52.5rem, 100vw)",
      children: React.createElement(Form),
    }),
  );
}
createRoot(fixtureRoot).render(
  React.createElement(MantineProvider, null, React.createElement(App)),
);

declare global {
  interface Window {
    mounts: number;
    savedWidth: number;
    originalInput: HTMLTextAreaElement;
    changeMode: (mode: "inline" | "drawer") => void;
    setOpened: (opened: boolean) => void;
    setFormOpened: (opened: boolean) => void;
  }
}

import type { Agent, Snapshot } from "../../src/types.ts";
import React from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import TerminalDock from "../../src/components/TerminalDock.tsx";
import { setToken } from "../../src/api.ts";
export function mount(shell: string) {
  window.reactRoot?.unmount();
  localStorage.setItem("codex.terminal.open", "true");
  localStorage.setItem(
    "codex.terminal.selected",
    JSON.stringify("shell:" + shell),
  );
  setToken("fixture-token");
  const agent: Agent = {
    id: "agent",
    name: "Fixture",
    source: "managed",
    status: "idle",
  };
  // This one-off fixture omits snapshot metadata and runtime collections.
  const data = { threads: [agent] } as Snapshot;
  const root = createRoot(document.getElementById("app")!);
  root.render(
    React.createElement(
      MantineProvider,
      null,
      React.createElement(TerminalDock, {
        data,
        agent,
        notify: (value) => window.notices.push(value),
      }),
    ),
  );
  window.reactRoot = root;
}

window.terminalInputRecoveryMount = mount;

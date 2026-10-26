import { fixtureSnapshot } from "../fixtures/snapshot.ts";
import type { Agent } from "../../src/types.ts";
import { required } from "../teams/helpers.ts";
import React from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import TerminalDock from "../../src/components/TerminalDock.tsx";
import { setToken } from "../../src/api.ts";
export function mount(shell: string) {
  window.terminalInputRecoveryRoot?.unmount();
  localStorage.setItem("codex.terminal.open", "true");
  localStorage.setItem(
    "codex.terminal.selected",
    JSON.stringify("shell:" + shell),
  );
  setToken("fixture-token");
  const agent: Agent = {
    created: 1,
    updated: 1,
    id: "agent",
    name: "Fixture",
    source: "managed",
    status: "idle",
  };
  const root = createRoot(required(document.getElementById("app")));
  root.render(
    React.createElement(
      MantineProvider,
      null,
      React.createElement(TerminalDock, {
        data: fixtureSnapshot([agent]),
        agent,
        notify: (value) => window.terminalInputRecoveryNotices.push(value),
      }),
    ),
  );
  window.terminalInputRecoveryRoot = root;
}

window.terminalInputRecoveryMount = mount;

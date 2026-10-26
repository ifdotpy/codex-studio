import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import "../../src/studio-theme.css";
import "../../src/appearance.css";
import TerminalDock from "../../src/components/TerminalDock.tsx";
import BackgroundTasks from "../../src/components/shell/BackgroundTasks.tsx";
import DraftVersions from "../../src/components/DraftVersions.tsx";
import type { Agent, BackgroundTask } from "../../src/types.ts";
import { fixtureSnapshot } from "./snapshot.ts";

declare global {
  interface Window {
    support: { initial: () => void; newer: () => void; scope: () => void };
  }
}

const task = (id: string, created: number, agent = "lead"): BackgroundTask => ({
  id,
  created,
  agent,
  kind: "command",
  name: "commandExecution",
  command: id,
  status: "running",
  tail: id,
});
function Fixture() {
  const [scope, setScope] = useState("lead");
  const [tasks, setTasks] = useState<BackgroundTask[]>([]);
  const [text, setText] = useState("Current");
  const [opened, setOpened] = useState(false);
  const agent = {
    id: "lead",
    rootId: "lead",
    isLead: true,
    name: "Lead",
    cwd: "/fixture",
  } satisfies Agent;
  const data = fixtureSnapshot(
    [agent, { ...agent, id: "other", rootId: "other" }],
    tasks,
  );
  window.support = {
    initial: () => setTasks([task("first", 10)]),
    newer: () => setTasks([task("new", 20), task("first", 10)]),
    scope: () => {
      setScope("other");
      setTasks([task("other-task", 30, "other")]);
    },
  };
  return (
    <MantineProvider>
      <button onClick={() => setOpened(true)}>Tasks</button>
      <BackgroundTasks
        opened={opened}
        close={() => setOpened(false)}
        data={data}
        leadId={scope}
        openAgent={() => {}}
        refresh={async () => {}}
        notify={() => {}}
      />
      <textarea
        aria-label="Draft text"
        value={text}
        onChange={(e) => setText(e.target.value)}
      />
      <DraftVersions
        versions={[
          {
            id: "branch",
            session: "lead",
            text: "Saved text",
            device: "recorded-device",
            updated: 1725926400000,
          },
          {
            id: "branch:conflict",
            session: "lead",
            text: "Alternative text",
            device: "recorded-device",
            updated: 1725926400000,
          },
        ]}
        useVersion={(version, mode) =>
          setText((previous) =>
            mode === "append" ? previous + "\n\n" + version.text : version.text,
          )
        }
        dismiss={() => {}}
      />
      <TerminalDock data={data} agent={agent} notify={() => {}} />
    </MantineProvider>
  );
}
const root = document.getElementById("root");
if (!root) throw new Error("Missing support fixture root");
createRoot(root).render(<Fixture />);

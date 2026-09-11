import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import "../../web/src/studio-theme.css";
import "../../web/src/appearance.css";
import TerminalDock from "../../web/src/components/TerminalDock";
import BackgroundTasks from "../../web/src/components/BackgroundTasks";
import DraftVersions from "../../web/src/components/DraftVersions";
const task = (id: string, created: number, agent = "lead") => ({
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
  const [tasks, setTasks] = useState<any[]>([]);
  const [text, setText] = useState("Current");
  const [opened, setOpened] = useState(false);
  const agent = {
    id: "lead",
    rootId: "lead",
    isLead: true,
    name: "Lead",
    cwd: "/fixture",
  };
  const data: any = {
    threads: [agent, { ...agent, id: "other", rootId: "other" }],
    runtime: { tasks, monitors: [], requests: [] },
  };
  (window as any).support = {
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
      <TerminalDock data={data} agent={agent as any} notify={() => {}} />
    </MantineProvider>
  );
}
createRoot(document.getElementById("root")!).render(<Fixture />);

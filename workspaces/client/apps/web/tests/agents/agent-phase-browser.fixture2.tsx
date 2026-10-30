import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { flushSync } from "react-dom";
import AgentPhase from "../../src/components/agents/AgentPhase.tsx";
import ChatStatus from "../../src/components/agents/ChatStatus.tsx";
import {
  chatWaitState,
  chatIndicators,
} from "../../src/components/chat-status/chatStatusModel.ts";
import type {
  Agent,
  BackgroundTask,
  Monitor,
  Snapshot,
} from "../../src/types.ts";
import { simpleRequest } from "../../src/approvalFixtures.ts";
import type { Approval } from "../../src/approvals.ts";
import "../../src/style.css";
import "../../src/workspace-layout.css";

declare global {
  interface Window {
    base: Agent;
    worker: (index: number) => Agent;
    command: (index: number) => BackgroundTask;
    monitor: (index: number) => Monitor;
    input: (id: string) => Approval;
    setData: (value: PhaseSnapshot) => void;
  }
}
const lead = {
  id: "lead",
  name: "Main",
  rootId: "lead",
  source: "managed",
  status: "waiting",
  inFlight: false,
  epoch: 2,
} satisfies Agent;
window.base = lead;
window.worker = (i) => ({
  ...lead,
  id: "child-" + i,
  name: "Worker " + i,
  parentId: "lead",
  status: "running",
  inFlight: true,
});
window.command = (i) => ({
  id: "cmd-" + i,
  agent: "lead",
  kind: "command",
  command: "npm run check " + i,
  status: "running",
  epoch: 2,
});
window.input = (id) => simpleRequest({ id, agent: "lead", epoch: 2 });
window.monitor = (i) => ({
  id: "monitor-" + i,
  agent: "lead",
  command: "watch build " + i,
  status: "running",
  epoch: 2,
});
export type PhaseSnapshot = Pick<Snapshot, "threads"> & {
  runtime: Pick<Snapshot["runtime"], "requests" | "tasks" | "monitors">;
};
const initial: PhaseSnapshot = {
  threads: [lead, window.worker(1), window.worker(2)],
  runtime: { requests: [], tasks: [], monitors: [] },
};
// The one-off waiting-state fixture omits unrelated snapshot fields and runtime collections.
function phaseSnapshot(value: unknown): Snapshot {
  return value as Snapshot;
}
function Fixture() {
  const [data, setData] = useState<PhaseSnapshot>(initial);
  window.setData = (value) => flushSync(() => setData(value));
  const agent = data.threads[0]!;
  const full = phaseSnapshot(data);
  const wait = chatWaitState(full, agent);
  const indicator = chatIndicators(full).get(agent.id);
  return (
    <main style={{ width: "100%", padding: 16, marginTop: 260 }}>
      <div id="indicator">
        <ChatStatus status={indicator} />
      </div>
      <AgentPhase agent={agent} connection={null} wait={wait} />
      <div id="anchor">Next content</div>
    </main>
  );
}
const root = document.getElementById("root")!;
createRoot(root).render(<Fixture />);

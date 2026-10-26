import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { flushSync } from "react-dom";
import AgentPhase from "../../src/components/agents/AgentPhase.tsx";
import ChatStatus from "../../src/components/agents/ChatStatus.tsx";
import {
  chatWaitState,
  chatIndicators,
} from "../../src/components/chat-status/chatStatusModel.ts";
import { snapshotFixture } from "../../src/snapshotFixture.ts";
import type {
  Agent,
  BackgroundTask,
  Monitor,
  Snapshot,
} from "../../src/types.ts";
import "../../src/style.css";
import "../../src/workspace-layout.css";

declare global {
  interface Window {
    agentPhaseFixture: {
      base: Agent;
      worker: (index: number) => Agent;
      command: (index: number) => BackgroundTask;
      monitor: (index: number) => Monitor;
      setData: (value: Pick<Snapshot, "threads" | "runtime">) => void;
    };
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
window.agentPhaseFixture = {
  base: lead,
  worker: (i) => ({
    ...lead,
    id: "child-" + i,
    name: "Worker " + i,
    parentId: "lead",
    status: "running",
    inFlight: true,
  }),
  command: (i) => ({
    id: "cmd-" + i,
    agent: "lead",
    kind: "command",
    command: "npm run check " + i,
    status: "running",
    epoch: 2,
  }),
  monitor: (i) => ({
    id: "monitor-" + i,
    agent: "lead",
    command: "watch build " + i,
    status: "running",
    epoch: 2,
  }),
  setData: () => {},
};
const initial = snapshotFixture([
  lead,
  window.agentPhaseFixture.worker(1),
  window.agentPhaseFixture.worker(2),
]);
function Fixture() {
  const [data, setData] =
    useState<Pick<Snapshot, "threads" | "runtime">>(initial);
  window.agentPhaseFixture.setData = (value) => flushSync(() => setData(value));
  const agent = data.threads[0];
  if (!agent) throw new Error("Missing agent phase lead");
  const full = { ...initial, ...data };
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
const root = document.getElementById("root");
if (!root) throw new Error("Missing agent phase root");
createRoot(root).render(<Fixture />);

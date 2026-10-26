import { useState } from "react";
import { createRoot } from "react-dom/client";
import { flushSync } from "react-dom";
import SessionActivity from "../../src/components/agents/SessionActivity.tsx";
import type { SessionActivityItem } from "../../src/components/agents/SessionActivity.tsx";
declare global {
  interface Window {
    sessionActivityFixture: {
      opened: SessionActivityItem[];
      time: number;
      activities: SessionActivityItem[];
      setActivities: (value: SessionActivityItem[]) => void;
    };
  }
}
window.sessionActivityFixture = {
  opened: [],
  time: Date.now(),
  activities: [],
  setActivities: () => {
    throw new Error("Fixture not mounted");
  },
};
const now = Date.now() / 1000;
window.sessionActivityFixture.opened = [];
window.sessionActivityFixture.time = Date.now();
Date.now = () => window.sessionActivityFixture.time;
const items: SessionActivityItem[] = [
  {
    id: "long-command",
    kind: "task",
    agentId: "paused-child",
    agentName: "webcrypto-globals",
    label: "Running command",
    command: "find /Users/igor " + "directory/".repeat(35),
    created: now - 14 * 3600,
    status: "running",
  },
  {
    id: "monitor-one",
    kind: "monitor",
    agentId: "lead",
    agentName: "Main chat",
    label: "Waiting for a monitor",
    command: "watch build",
    created: now - 120,
    status: "running",
  },
  {
    id: "worker-one",
    kind: "agent",
    agentId: "worker-one",
    agentName: "Worker one",
    label: "Working",
    status: "running",
  },
  {
    id: "tool-two",
    kind: "task",
    agentId: "worker-two",
    agentName: "Worker with a very long name that must fit the mobile viewport",
    label: "Using tools",
    created: now - 30,
    status: "starting",
  },
];
function Fixture() {
  const [activities, setActivities] = useState(items);
  window.sessionActivityFixture.setActivities = (value) =>
    flushSync(() => setActivities(value));
  window.sessionActivityFixture.activities = activities;
  return (
    <>
      <header>Main chat</header>
      <SessionActivity
        activities={activities}
        onOpen={(value) => window.sessionActivityFixture.opened.push(value)}
      />
      <main>Conversation</main>
    </>
  );
}
const root = document.getElementById("root");
if (!root) throw new Error("Missing fixture root");
createRoot(root).render(<Fixture />);

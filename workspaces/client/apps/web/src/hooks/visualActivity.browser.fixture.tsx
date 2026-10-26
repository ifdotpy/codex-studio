declare global {
  interface Window {
    commits: Record<string, number>;
    rateInterests: Set<
      Parameters<Parameters<typeof configureTokenRateStream>[0]>[0]
    >;
    rate: (id: string, value: number) => void;
    workerRate: (value: number) => void;
    unmount: () => void;
    visualMetrics: { ticks: number; observers: number };
    activeVisualTimers: () => number;
    testingHidden: boolean;
    setTestingHidden: (hidden: boolean) => void;
  }
}

import React from "react";
import { createRoot } from "react-dom/client";
import TokenRate from "../components/TokenRate.tsx";
import ReasoningDuration from "../components/conversation/transcript/ReasoningDuration.tsx";
import ChatStatus from "../components/agents/ChatStatus.tsx";
import { useTeamTokenRateStream } from "./useTeamTokenRateStream.ts";
import {
  receiveWorkspaceTokenRates,
  configureTokenRateStream,
} from "../usage/tokenRate.ts";
import "../visual-activity.css";
import "../components/team-navigation.css";
const e = React.createElement;
window.commits = {};
window.rateInterests = new Set();
configureTokenRateStream((listener) => {
  window.rateInterests.add(listener);
  return () => window.rateInterests.delete(listener);
});
const records = new Map<string, import("../usage/tokenRate.ts").TokenRate>();
window.rate = (id, value) => {
  records.set(id, {
    agentId: id,
    rootId: id === "worker" ? "team" : id,
    turnId: "turn",
    active: true,
    estimated: false,
    rate: value,
    outputTokens: 100,
  });
  receiveWorkspaceTokenRates({ records: [...records.values()] });
};
window.workerRate = (value) => window.rate("worker", value);
for (const id of ["near", "far", "closed"]) window.rate(id, 20);
const card = (id: string, reason = true) =>
  e(
    React.Profiler,
    {
      id,
      onRender: () => (window.commits[id] = (window.commits[id] || 0) + 1),
    },
    e(
      "div",
      { id, className: "card" },
      e(ChatStatus, { status: { kind: "working", label: "Working" } }),
      e(TokenRate, { agent: { id, turnId: "turn" } }),
      reason &&
        e(ReasoningDuration, {
          item: {
            id: "reason:" + id,
            reasoningMs: 3000,
            reasoningSince: 100,
            reasoningObservedAt: 100,
          },
        }),
    ),
  );
function TeamRateInterest() {
  useTeamTokenRateStream("team", true);
  return null;
}
const root = createRoot(document.querySelector("#root")!);
root.render(
  e(
    React.StrictMode,
    null,
    e(TeamRateInterest),
    card("near"),
    card("empty", false),
    e(
      "div",
      { id: "team" },
      e(
        "span",
        { className: "worker-meta" },
        "Worker ",
        e(TokenRate, {
          agent: { id: "worker", rootId: "team" },
          variant: "worker",
        }),
      ),
    ),
    e(
      "details",
      { id: "fold" },
      e("summary", null, "Show work"),
      card("closed"),
    ),
    card("far"),
  ),
);
window.unmount = () => root.unmount();

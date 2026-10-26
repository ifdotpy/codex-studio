declare global {
  interface Window {
    visualActivityFixture: {
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
    };
  }
}

import React from "react";
import { createRoot } from "react-dom/client";
import TokenRate from "../components/TokenRate.tsx";
import ReasoningDuration from "../components/conversation/transcript/ReasoningDuration.tsx";
import ChatStatus from "../components/agents/ChatStatus.tsx";
import {
  receiveWorkspaceTokenRates,
  configureTokenRateStream,
} from "../usage/tokenRate.ts";
import "../visual-activity.css";
import "../components/team-navigation.css";
const e = React.createElement;
window.visualActivityFixture.commits = {};
window.visualActivityFixture.rateInterests = new Set();
configureTokenRateStream((listener) => {
  window.visualActivityFixture.rateInterests.add(listener);
  return () => window.visualActivityFixture.rateInterests.delete(listener);
});
const records = new Map<string, import("../usage/tokenRate.ts").TokenRate>();
window.visualActivityFixture.rate = (id, value) => {
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
window.visualActivityFixture.workerRate = (value) =>
  window.visualActivityFixture.rate("worker", value);
for (const id of ["near", "far", "closed"])
  window.visualActivityFixture.rate(id, 20);
const card = (id: string, reason = true) =>
  e(
    React.Profiler,
    {
      id,
      onRender: () =>
        (window.visualActivityFixture.commits[id] =
          (window.visualActivityFixture.commits[id] || 0) + 1),
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
            role: "assistant",
            text: "",
            reasoningMs: 3000,
            reasoningSince: 100,
            reasoningObservedAt: 100,
          },
        }),
    ),
  );
const element = document.querySelector("#root");
if (!element) throw new Error("Missing visual fixture root");
const root = createRoot(element);
root.render(
  e(
    React.StrictMode,
    null,
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
window.visualActivityFixture.unmount = () => root.unmount();

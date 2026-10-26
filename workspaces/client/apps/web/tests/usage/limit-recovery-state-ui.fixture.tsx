import type { Agent, Message } from "../../src/types.ts";
import { fixtureRoot } from "./usage-fixture.ts";

import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import {
  NativeError,
  NativeNotice,
} from "../../src/components/NativeNotice.tsx";
import Usage from "../../src/components/Usage.tsx";
const now = Number(new URLSearchParams(location.search).get("now"));
const error = {
  codexErrorInfo: "usageLimitExceeded",
  message: "Historical quota failure",
};
const agent: Agent = {
  id: "lumina",
  rootId: "lumina",
  accountKey: "work",
  threadId: "native-lumina",
  model: "gpt-6-astra",
  provider: "codex",
  error,
  nativeLimitErrorAt: now - 10,
  status: "failed",
  isLead: true,
};
const item: Message = {
  id: "past-error",
  role: "system",
  nativeNotice: "error",
  nativeError: error,
  text: error.message,
};
export type LimitSnapshot = {
  label: string;
  limits: Parameters<typeof Usage>[0]["limits"];
  agentId?: string;
  accountKey?: string;
  threadId?: string;
  turnId?: Agent["turnId"];
  lastCompletedTurn?: Agent["lastCompletedTurn"];
  episode?: number;
  mountKey?: number;
};
declare global {
  interface Window {
    limitRecoveryFixture: {
      actions: string[];
      applySerialized: (serialized: string) => void;
    };
  }
}
window.limitRecoveryFixture = { actions: [], applySerialized: () => {} };
const action = () => {
  window.limitRecoveryFixture.actions.push("action");
};
function parseSnapshot(serialized: string): LimitSnapshot | null {
  // This fixture owns both the session document writer and reader.
  return JSON.parse(serialized) as LimitSnapshot | null;
}
function Fixture() {
  const [snapshot, setSnapshot] = useState<LimitSnapshot>(
    () =>
      parseSnapshot(sessionStorage.getItem("fixture-initial") || "null") || {
        label: "initial",
        limits: null,
      },
  );
  window.limitRecoveryFixture.applySerialized = (serialized) => {
    const next = parseSnapshot(serialized);
    if (next) setSnapshot(next);
  };
  const currentAgent = {
    ...agent,
    id: snapshot.agentId || agent.id,
    accountKey: snapshot.accountKey || agent.accountKey,
    threadId: snapshot.threadId || agent.threadId,
    turnId: snapshot.turnId,
    lastCompletedTurn: snapshot.lastCompletedTurn,
    nativeLimitErrorAt: snapshot.episode ?? agent.nativeLimitErrorAt,
  };
  return (
    <MantineProvider>
      <main data-case={snapshot.label}>
        <section id="current">
          <NativeError
            key={snapshot.mountKey}
            agent={currentAgent}
            limits={snapshot.limits}
            openLimits={action}
          />
        </section>
        <section id="history">
          <NativeNotice item={item} />
        </section>
        <section style={{ position: "fixed", bottom: 20, right: 20 }}>
          <Usage
            stateDir=""
            key={snapshot.mountKey}
            agent={currentAgent}
            limits={snapshot.limits}
            reload={action}
            opened={true}
            onChange={action}
            limitsLoading={false}
          />
        </section>
        <output id="saved-error">{JSON.stringify(agent.error)}</output>
      </main>
    </MantineProvider>
  );
}
createRoot(fixtureRoot()).render(<Fixture />);

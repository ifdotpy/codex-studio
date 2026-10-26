import type { components } from "../../src/generated/api.ts";
import type { Agent } from "../../src/types.ts";
import type { PostBody, GetResult } from "../../src/apiTypes.ts";
import { fixtureSnapshot } from "../fixtures/snapshot.ts";
import {
  fixtureAccount,
  fixtureNotReady,
  fixtureFetchInput,
  fixturePostBody,
} from "./fixture-helpers.ts";
type ChatSettingsHarness = {
  calls: PostBody<"/api/conversation">[];
  agent: Agent;
  updateAgent: (agent: Agent) => void;
};
declare global {
  interface Window {
    chatSettingsFixture: ChatSettingsHarness;
  }
}
import { required } from "./fixture-helpers.ts";
const fixtureRoot = document.getElementById("root");
if (!fixtureRoot) throw new Error("Missing fixture root");
import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { flushSync } from "react-dom";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import "../../src/style.css";
import "../../src/appearance.css";
import "../../src/design/state.css";
import { theme } from "../../src/theme.ts";
import { ChatSettingsDialog } from "../../src/components/agents/ChatSettingsDialog.tsx";
const names = [
  "gpt-6-astra",
  "gpt-6.1-sol",
  "gpt-6-luna",
  "gpt-5.6-sol",
  "gpt-5.6-luna",
  "gpt-5.6-terra",
  "gpt-6-sol",
];
const models = names.map((model, index) => ({
  model,
  provider: "codex",
  isDefault: !index,
  description: "Coding and reasoning for " + model,
  supportedReasoningEfforts: [
    "low",
    "medium",
    "high",
    "xhigh",
    "max",
    "ultra",
  ].map((reasoningEffort) => ({ reasoningEffort })),
  serviceTiers: [{ id: "priority" }],
  availableAccessPrograms: { cyber: ["standard", "daybreakBlue"] },
}));
const accounts = Array.from({ length: 5 }, (_, i) =>
  fixtureAccount({
    id: i ? "account-" + i : "default",
    label: "Work " + (i + 1),
    email: "operator" + (i + 1) + "@example.com",
    status: "ready",
    provider: "codex",
  }),
);
const accountsData: GetResult<"/api/accounts"> = {
  accounts,
  archivedAccounts: [],
  defaultAccountKey: "default",
  defaultAccountGroups: [],
  logins: [],
  supportsDelete: true,
  supportsDisconnect: true,
};
const initial: Agent = {
  id: "lead",
  accountKey: "default",
  provider: "codex",
  name: "Lead",
  source: "managed",
  model: names[0],
  effort: "high",
  isLead: true,
  status: "idle",
  created: 1,
  yoloMode: false,
  subagentConcurrencyVersion: 2,
  concurrency: 8,
  agentModeRevision: 1,
  workerDefaults: {
    fastMode: false,
    model: "gpt-6-luna",
    effort: "high",
    delegationDepth: 1,
  },
  reviewDefaults: { model: null, effort: null },
};
window.chatSettingsFixture = {
  calls: [],
  agent: initial,
  updateAgent: fixtureNotReady,
};
const nativeFetch = window.fetch;
window.fetch = async (input, options) => {
  const { url, options: requestOptions } = await fixtureFetchInput(
    input,
    options,
  );
  const u = new URL(url, location.origin);
  let result: unknown;
  if (u.pathname === "/api/limits") {
    const key = u.searchParams.get("account_key");
    result = {
      accountKey: key || "",
      at: Date.now() / 1000,
      data: {
        rateLimits: {
          primary: {
            usedPercent: 20,
            windowDurationMins: 300,
            resetsAt: Date.now() / 1000 + 3600,
          },
        },
      },
    } satisfies components["schemas"]["ModelCatalogResponse"];
  } else if (u.pathname === "/api/models")
    result = {
      data: models,
    } satisfies components["schemas"]["ModelCatalogResponse"];
  else if (u.pathname === "/api/desktop")
    result = {
      browser: {
        enabled: false,
        reason: "Browser access is disabled for this account.",
      },
    } satisfies Pick<components["schemas"]["DesktopResponse"], "browser">;
  else if (u.pathname === "/api/accounts") result = accountsData;
  else if (u.pathname === "/api/conversation") {
    const body = fixturePostBody<"/api/conversation">(requestOptions?.body);
    window.chatSettingsFixture.calls.push(body);
    const next = structuredClone(window.chatSettingsFixture.agent);
    if (body.worker_defaults)
      next.workerDefaults = Object.assign(
        { model: null, effort: null, fastMode: false },
        {
          ...next.workerDefaults,
          model: body.worker_defaults.model,
          effort: body.worker_defaults.effort,
          delegationDepth: body.worker_defaults.delegation_depth,
          accountKey: body.worker_defaults.account_key,
          fastMode: body.worker_defaults.fast_mode,
        },
      );
    else if (body.review_defaults) next.reviewDefaults = body.review_defaults;
    else if (body.excluded_account_keys)
      next.excludedAccountKeys = body.excluded_account_keys;
    else if ("subagent_concurrency" in body) {
      next.concurrency = body.subagent_concurrency;
      next.agentModeRevision = required(next.agentModeRevision) + 1;
    } else if ("yolo_mode" in body) next.yoloMode = body.yolo_mode;
    else
      Object.assign(next, {
        model: body.model,
        effort: body.effort,
        fastMode: body.fast_mode,
        daybreakEnabled: body.daybreak_enabled,
      });
    window.chatSettingsFixture.updateAgent(next);
    result = next;
  }
  if (!result) return nativeFetch(input, options);
  return new Response(JSON.stringify(result), {
    headers: { "Content-Type": "application/json" },
  });
};
function Fixture() {
  const [agent, setAgent] = useState<Agent>(initial),
    [opened, setOpened] = useState(true);
  window.chatSettingsFixture.agent = agent;
  window.chatSettingsFixture.updateAgent = (value) =>
    flushSync(() => setAgent(value));
  const state = {
    data: accountsData,
    setData: () => {},
    error: "",
    refresh: async () => accountsData,
    scope: "fixture",
  };
  return (
    <MantineProvider
      theme={theme}
      forceColorScheme={
        new URLSearchParams(location.search).get("theme") === "dark"
          ? "dark"
          : "light"
      }
    >
      <ChatSettingsDialog
        settingsOpen={opened}
        setSettingsOpen={setOpened}
        accountModalOpen={false}
        agent={agent}
        lead={agent}
        accounts={state}
        agentModels={{ models, loading: false, error: "", retry: () => {} }}
        data={{ ...fixtureSnapshot([]), stateDir: "execution-preview" }}
        workspaceId="1234567890abcdef1234567890abcdef"
        refresh={async () => {}}
        notify={() => {}}
        creating={false}
        newChat={() => {}}
        project={() => {}}
        selectedAccount={accounts[0]}
        setClaudeLoginKey={() => {}}
      />
    </MantineProvider>
  );
}
createRoot(fixtureRoot).render(<Fixture />);

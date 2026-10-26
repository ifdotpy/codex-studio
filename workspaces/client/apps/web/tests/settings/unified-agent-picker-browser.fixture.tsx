import type { Snapshot, Agent } from "../../src/types.ts";
import type { Account, AccountsState } from "../../src/components/Accounts.tsx";
import type { WorkerModelInfo } from "../../src/components/agents/WorkerModelPicker.tsx";
import type { PostBody } from "../../src/apiTypes.ts";
import type { components } from "../../src/generated/api.ts";
import { fixtureSnapshot } from "../fixtures/snapshot.ts";
import {
  fixtureNotReady,
  fixtureFetchInput,
  fixturePostBody,
} from "./fixture-helpers.ts";
type PickerCall = Omit<Partial<PostBody<"/api/conversation">>, "id"> &
  Pick<Partial<PostBody<"/api/agents/account-transfer">>, "id"> &
  Partial<Pick<components["schemas"]["ProjectWriteRequest"], "account_keys">> &
  Partial<
    Pick<components["schemas"]["PeerTeamRadioCreateRequest"], "participants">
  > & {
    endpoint?: string;
    path?: components["schemas"]["ProjectLocationRequest"]["path"];
    key?: string;
  };
export type PickerScene =
  | "picker"
  | "rows"
  | "chat-account"
  | "project"
  | "shared";
type UnifiedPickerHarness = {
  ready: boolean;
  required: typeof required;
  whitelist: (value: Parameters<typeof writeModelWhitelist>[0]) => void;
  calls: PickerCall[];
  agent: Agent;
  updateAgent: (agent: Agent) => void;
  scene: (scene: PickerScene) => void;
  scheme: (scheme: "light" | "dark") => void;
  reset: () => void;
  created: string;
};
declare global {
  interface Window {
    unifiedPickerFixture: UnifiedPickerHarness;
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
import { theme } from "../../src/theme.ts";
import "../../src/style.css";
import { ExecutionSettings } from "../../src/components/agents/ExecutionSettings.tsx";
import { SettingsRow } from "../../src/components/ui/primitives.tsx";
import "../../src/appearance.css";
import { AccountTiles } from "../../src/components/AccountTiles.tsx";
import ProjectAccount from "../../src/components/ProjectAccount.tsx";
import SharedChatCreate from "../../src/components/SharedChatCreate.tsx";
import { writeModelWhitelist } from "../../src/components/models/modelWhitelist.ts";

const accounts: Account[] = Array.from({ length: 7 }, (_, i) => ({
  id: "account-" + i,
  home: "/fixture/" + i,
  label: i < 3 ? "Work " + (i + 1) : "Personal " + (i - 2),
  email: "fixture" + i + "@example.com",
  source: "fixture",
  provider: i < 4 ? "codex" : "claude",
  status: i === 6 ? "signedOut" : "ready",
}));
const codex = ["gpt-6-astra", "gpt-6-luna"].map((model, i) => ({
  model,
  displayName: i ? "Luna" : "Astra",
  isDefault: !i,
  provider: "codex",
  defaultReasoningEffort: "medium",
  supportedReasoningEfforts: ["low", "medium", "high"].map(
    (reasoningEffort) => ({ reasoningEffort }),
  ),
  serviceTiers: i ? [] : [{ id: "priority" }],
  availableAccessPrograms: {
    cyber: i ? ["standard"] : ["standard", "daybreakBlue"],
  },
}));
const claudeRow = (
  model: string,
  displayName: string,
  extra: Partial<Pick<WorkerModelInfo, "isDefault" | "resolvedModel">>,
): WorkerModelInfo & components["schemas"]["ProviderModel"] => ({
  model,
  displayName,
  provider: "claude",
  defaultReasoningEffort: "medium",
  supportedReasoningEfforts: ["low", "medium", "high"].map(
    (reasoningEffort) => ({ reasoningEffort }),
  ),
  serviceTiers: [],
  ...extra,
});
const claude = [
  claudeRow("default", "Claude · Default (recommended)", {
    isDefault: true,
    resolvedModel: "claude-opus-5-5",
  }),
  claudeRow("opus", "Claude · Opus 5.5", { resolvedModel: "claude-opus-5-5" }),
  claudeRow("claude-sonnet-5-5", "Claude Sonnet 5.5", {}),
];
const initial: Agent = {
  id: "lead",
  accountKey: "account-0",
  provider: "codex",
  name: "Lead",
  source: "managed",
  model: "gpt-6-astra",
  effort: "medium",
  fastMode: false,
  daybreakEnabled: false,
  isLead: true,
  status: "idle",
  created: 1,
  yoloMode: false,
  nextTurnSettingsSupported: true,
  workerDefaults: { model: "gpt-6-luna", effort: "high", fastMode: false },
  reviewDefaults: { model: null, effort: null },
};
window.unifiedPickerFixture = {
  ready: false,
  required,
  whitelist: (value) => writeModelWhitelist(value, "local"),
  calls: [],
  agent: initial,
  updateAgent: fixtureNotReady,
  scene: fixtureNotReady,
  scheme: fixtureNotReady,
  reset: fixtureNotReady,
  created: "",
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
    const key = u.searchParams.get("account_key"),
      i = Number(key?.split("-")[1]);
    result = {
      accountKey: key || "",
      at: Date.now() / 1000,
      data: {
        rateLimits: {
          limitId: i < 4 ? "codex" : "claude",
          primary: {
            usedPercent: required([20, 60, 95, 100, 25, 70, 100][i]),
            windowDurationMins: 300,
            resetsAt: Date.now() / 1000 + 3600,
          },
        },
      },
    } satisfies components["schemas"]["AccountRateLimitsDto"];
  } else if (u.pathname === "/api/models") {
    result = {
      data: u.searchParams.get("workers")
        ? "1" === u.searchParams.get("workers")
          ? [...codex, ...claude]
          : codex
        : Number(u.searchParams.get("account_key")?.split("-")[1]) >= 4
          ? claude
          : codex,
    } satisfies components["schemas"]["ModelCatalogResponse"];
  } else if (requestOptions?.method === "POST") {
    if (u.pathname === "/api/conversation") {
      const body = fixturePostBody<"/api/conversation">(requestOptions.body);
      window.unifiedPickerFixture.calls.push({ endpoint: u.pathname, ...body });
      const next = structuredClone(window.unifiedPickerFixture.agent);
      if (body.worker_defaults)
        next.workerDefaults = Object.assign(
          { model: null, effort: null, fastMode: false },
          {
            model: body.worker_defaults.model,
            effort: body.worker_defaults.effort,
            fastMode: body.worker_defaults.fast_mode,
            daybreakEnabled: body.worker_defaults.daybreak_enabled ?? false,
            accountKey: body.worker_defaults.account_key,
          },
        );
      else if (body.review_defaults) next.reviewDefaults = body.review_defaults;
      else
        Object.assign(next, {
          model: body.model,
          effort: body.effort,
          fastMode: body.fast_mode,
          daybreakEnabled: body.daybreak_enabled,
        });
      window.unifiedPickerFixture.updateAgent(next);
      result = next;
    } else if (u.pathname === "/api/agents/account-transfer") {
      const body = fixturePostBody<"/api/agents/account-transfer">(
        requestOptions.body,
      );
      window.unifiedPickerFixture.calls.push({ endpoint: u.pathname, ...body });
      result = {
        id: body.request_id,
        leadId: body.id,
        scope: body.scope,
        status: "completed",
        targetAccountKey: body.account_key,
        total: 0,
        moved: 0,
        completed: 0,
      };
      window.unifiedPickerFixture.updateAgent({
        ...window.unifiedPickerFixture.agent,
        workerDefaults: {
          accountKey: body.account_key,
          model:
            Number(required(body.account_key).split("-")[1]) >= 4
              ? required(claude[0]).model
              : required(codex[0]).model,
          effort: "medium",
          fastMode: false,
        },
      });
    } else if (u.pathname === "/api/projects") {
      const body = fixturePostBody<"/api/projects">(requestOptions.body);
      window.unifiedPickerFixture.calls.push({ endpoint: u.pathname, ...body });
      result = {
        id: "project",
        path: "/fixture/project",
        name: "Project",
        created: 1,
        accountKey: "account_key" in body ? body.account_key : undefined,
        accountKeys: "account_keys" in body ? body.account_keys : undefined,
        accountRevision: 1,
      };
    } else if (u.pathname === "/api/peer-teams") {
      const body = fixturePostBody<"/api/peer-teams">(requestOptions.body);
      window.unifiedPickerFixture.calls.push({ endpoint: u.pathname, ...body });
      result = { room: { id: "shared" } };
    }
  }
  if (result === undefined) return nativeFetch(url, requestOptions);
  return new Response(JSON.stringify(result), {
    headers: { "Content-Type": "application/json" },
  });
};
const accountsData = {
  accounts,
  defaultAccountKey: "account-0",
  archivedAccounts: [],
  defaultAccountGroups: [],
  logins: [],
  supportsDelete: false,
  supportsDisconnect: false,
} satisfies AccountsState;
function Fixture() {
  window.unifiedPickerFixture.ready = true;
  const [agent, setAgent] = useState<Agent>(initial),
    [scene, setScene] = useState<PickerScene>("picker"),
    [scheme, setScheme] = useState<"light" | "dark">("dark"),
    [account, setAccount] = useState("account-0");
  window.unifiedPickerFixture.agent = agent;
  window.unifiedPickerFixture.updateAgent = (value) =>
    flushSync(() => setAgent(value));
  window.unifiedPickerFixture.scene = (value) =>
    flushSync(() => setScene(value));
  window.unifiedPickerFixture.scheme = (value) =>
    flushSync(() => setScheme(value));
  window.unifiedPickerFixture.reset = () =>
    flushSync(() => {
      setAgent(initial);
      setScene("picker");
      window.unifiedPickerFixture.calls = [];
    });
  const data: Snapshot = fixtureSnapshot([]);
  data.stateDir = "fixture";
  data.runtime.projects = [
    { id: "project", path: "/fixture/project", name: "Project", created: 1 },
  ];
  return (
    <MantineProvider theme={theme} forceColorScheme={scheme}>
      <main style={{ padding: 20, maxWidth: 720, margin: "0 auto" }}>
        <h2>Agent setup</h2>
        {scene === "picker" ? (
          <ExecutionSettings
            agent={agent}
            accounts={accounts}
            catalog={{
              models: agent.provider === "claude" ? claude : codex,
              loading: false,
              error: "",
              retry: () => {},
            }}
            onAccountChange={(key) => {
              window.unifiedPickerFixture.calls.push({
                path: "account-selection",
                key,
              });
              const provider = required(
                accounts.find((a) => a.id === key),
              ).provider;
              setAgent({
                ...agent,
                accountKey: key,
                provider,
                model:
                  provider === "claude"
                    ? required(claude[0]).model
                    : required(codex[0]).model,
                effort: "medium",
              });
            }}
            refresh={async () => {}}
          />
        ) : scene === "rows" ? (
          <div className="chat-settings-panel chat-settings-rows">
            {(["orchestrator", "worker", "review"] as const).map((role) => (
              <SettingsRow
                key={role}
                label={
                  role === "orchestrator"
                    ? "Model"
                    : role === "worker"
                      ? "Workers"
                      : "Review"
                }
              >
                <ExecutionSettings
                  settingsRow
                  initialRole={role}
                  agent={agent}
                  accounts={accounts}
                  catalog={{
                    models: agent.provider === "claude" ? claude : codex,
                    loading: false,
                    error: "",
                    retry: () => {},
                  }}
                  onAccountChange={() => {}}
                  refresh={async () => {}}
                />
              </SettingsRow>
            ))}
          </div>
        ) : scene === "chat-account" ? (
          <AccountTiles
            label="Account"
            accounts={accounts}
            value={account}
            onChange={setAccount}
          />
        ) : scene === "project" ? (
          <ProjectAccount
            path="/fixture/project"
            project={{
              id: "project",
              path: "/fixture/project",
              name: "Project",
              created: 1,
              accountKey: "account-0",
              accountKeys: accounts.map((a) => a.id),
            }}
            defaultAccountKey="account-0"
            accounts={accountsData}
            saved={async () => {}}
          />
        ) : (
          <SharedChatCreate
            data={data}
            accounts={accountsData}
            initialPath="/fixture/project"
            refresh={async () => {}}
            created={(id) => {
              window.unifiedPickerFixture.created = id;
            }}
          />
        )}
      </main>
    </MantineProvider>
  );
}
createRoot(fixtureRoot).render(<Fixture />);

import type { Agent } from "../../src/types.ts";
import type { WorkerModelInfo } from "../../src/components/agents/WorkerModelPicker.tsx";
import type { GetOptions } from "../../src/api.ts";
import type { PostBody, PostResult } from "../../src/apiTypes.ts";
import {
  fixtureNotReady,
  fixtureFetchInput,
  fixturePostBody,
  fixtureAccount,
} from "./fixture-helpers.ts";
export type CanonicalReset = {
  agent?: Partial<
    Pick<
      Agent,
      | "id"
      | "accountKey"
      | "model"
      | "effort"
      | "fastMode"
      | "daybreakEnabled"
      | "status"
      | "workerDefaults"
      | "reviewDefaults"
      | "accountTransfer"
      | "pendingSettings"
      | "pendingSettingsAccountKey"
      | "isLead"
      | "provider"
      | "yoloMode"
    >
  >;
  defaults?: boolean;
};
type CanonicalHarness = {
  ready: boolean;
  required: typeof required;
  catalogRequests: string[];
  calls: PostBody<"/api/conversation">[];
  transfers: PostBody<"/api/agents/account-transfer">[];
  transferFail: boolean;
  transferHold: boolean;
  transferReply: Partial<PostResult<"/api/agents/account-transfer">> | null;
  pendingTransferProjection: Agent | null;
  reply: Agent | null;
  fail: string | null;
  hold: boolean;
  refreshFailure: boolean;
  refreshHold: boolean;
  refreshCount: number;
  catalogHold: boolean;
  catalogRelease: (() => void) | null;
  release: (() => void) | null;
  refreshRelease: (() => void) | null;
  transferRelease: (() => void) | null;
  serverAccount: string | null;
  agent: Agent;
  setAgent: (value: Agent) => void;
  reset: (options?: CanonicalReset) => void;
};
declare global {
  interface Window {
    canonicalSettingsFixture: CanonicalHarness;
  }
}
import { required } from "./fixture-helpers.ts";
const fixtureRoot = document.getElementById("root");
if (!fixtureRoot) throw new Error("Missing fixture root");
import {
  API_SCHEMA_HASH,
  API_SCHEMA_HASH_HEADER,
} from "../../src/generated/apiSchema.ts";
const fixtureSchemaHeaders = {
  "Content-Type": "application/json",
  [API_SCHEMA_HASH_HEADER]: API_SCHEMA_HASH,
};
import React, { useState } from "react";
import type { ContextType } from "react";
import { createRoot } from "react-dom/client";
import { flushSync } from "react-dom";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import { ExecutionSettings } from "../../src/components/agents/ExecutionSettings.tsx";
import { ModelCatalogReader } from "../../src/components/agents/WorkerModelPicker.tsx";
import { get } from "../../src/api.ts";
const fixtureModelRead = (
  options: Pick<GetOptions<"/api/models">, "query" | "signal">,
) => get("/api/models", options);
const initial: Agent = {
  id: "first",
  accountKey: "default",
  name: "First",
  source: "managed",
  model: "gpt-6-astra",
  effort: "low",
  fastMode: false,
  isLead: true,
  status: "idle",
  created: 1,
  yoloMode: false,
  nextTurnSettingsSupported: true,
};
const models: WorkerModelInfo[] = [
  "gpt-6-astra",
  "gpt-5.6-sol",
  "gpt-5.6-luna",
  "gpt-6-luna",
].map((model) => ({
  model,
  supportedReasoningEfforts: ["low", "medium", "high", "max"].map(
    (reasoningEffort) => ({ reasoningEffort }),
  ),
  serviceTiers: [{ id: "priority" }],
  availableAccessPrograms: { cyber: ["standard", "daybreakBlue"] },
}));
window.canonicalSettingsFixture = {
  ready: false,
  required,
  catalogRequests: [],
  calls: [],
  transfers: [],
  transferFail: false,
  transferHold: false,
  transferReply: null,
  pendingTransferProjection: null,
  reply: null,
  fail: null,
  hold: false,
  refreshFailure: false,
  refreshHold: false,
  refreshCount: 0,
  catalogHold: false,
  catalogRelease: null,
  release: null,
  refreshRelease: null,
  transferRelease: null,
  serverAccount: null,
  agent: initial,
  setAgent: fixtureNotReady,
  reset: fixtureNotReady,
};
window.canonicalSettingsFixture.calls = [];
window.canonicalSettingsFixture.transfers = [];
window.canonicalSettingsFixture.transferFail = false;
window.canonicalSettingsFixture.transferHold = false;
window.canonicalSettingsFixture.transferReply = null;
window.canonicalSettingsFixture.pendingTransferProjection = null;
window.canonicalSettingsFixture.reply = null;
window.canonicalSettingsFixture.fail = null;
window.canonicalSettingsFixture.hold = false;
window.canonicalSettingsFixture.refreshFailure = false;
window.canonicalSettingsFixture.refreshHold = false;
window.canonicalSettingsFixture.refreshCount = 0;
const fixtureResponse = (value: unknown, init: ResponseInit = {}) =>
  new Response(JSON.stringify(value), {
    ...init,
    headers: {
      ...fixtureSchemaHeaders,
      ...init.headers,
    },
  });
const nativeFetch = window.fetch;
window.fetch = async (input, options) => {
  const { url, options: requestOptions } = await fixtureFetchInput(
    input,
    options,
  );
  if (String(url).startsWith("/api/models?")) {
    window.canonicalSettingsFixture.catalogRequests.push(url);
    if (window.canonicalSettingsFixture.catalogHold)
      await new Promise<void>(
        (resolve) => (window.canonicalSettingsFixture.catalogRelease = resolve),
      );
    return fixtureResponse({
      data: String(url).includes("account_key=claude")
        ? [
            {
              model: "claude-opus-4-6",
              provider: "claude",
              displayName: "Claude Opus 4.6",
              isDefault: true,
              supportedReasoningEfforts: [{ reasoningEffort: "high" }],
            },
          ]
        : models,
    });
  }
  if (url == "/api/agents/account-transfer") {
    const body = fixturePostBody<"/api/agents/account-transfer">(
      requestOptions?.body,
    );
    window.canonicalSettingsFixture.transfers.push(body);
    if (window.canonicalSettingsFixture.transferHold)
      await new Promise<void>(
        (resolve) =>
          (window.canonicalSettingsFixture.transferRelease = resolve),
      );
    if (window.canonicalSettingsFixture.transferFail)
      throw new TypeError("Connection lost");
    const result = window.canonicalSettingsFixture.transferReply || {
      id: body.request_id,
      leadId: "first",
      scope: body.scope,
      status: "completed",
      targetAccountKey: body.account_key,
      completed: 2,
      moved: 2,
      total: 3,
      leftOnSource: [
        {
          id: "w-claude",
          name: "Claude worker",
          provider: "claude",
          reason: "Uses claude",
        },
      ],
    };
    window.canonicalSettingsFixture.pendingTransferProjection = {
      ...window.canonicalSettingsFixture.agent,
      workerDefaults: {
        model: null,
        effort: null,
        fastMode: false,
        ...window.canonicalSettingsFixture.agent.workerDefaults,
        accountKey: body.account_key,
      },
    };
    return fixtureResponse(result);
  }
  if (url != "/api/conversation") return nativeFetch(url, requestOptions);
  window.canonicalSettingsFixture.calls.push(
    fixturePostBody<"/api/conversation">(requestOptions?.body),
  );
  if (
    window.canonicalSettingsFixture.serverAccount &&
    required(window.canonicalSettingsFixture.calls.at(-1))
      .expected_account_key !== window.canonicalSettingsFixture.serverAccount
  )
    return fixtureResponse(
      { error: "The account changed. Select the model again" },
      { status: 409 },
    );
  if (window.canonicalSettingsFixture.hold)
    await new Promise<void>(
      (resolve) => (window.canonicalSettingsFixture.release = resolve),
    );
  if (window.canonicalSettingsFixture.fail === "network")
    throw new TypeError("Connection lost");
  return fixtureResponse(
    window.canonicalSettingsFixture.fail
      ? { error: "Rejected" }
      : window.canonicalSettingsFixture.reply,
    {
      status: window.canonicalSettingsFixture.fail ? 409 : 200,
    },
  );
};
const team: Agent[] = [
  {
    id: "w1",
    name: "Codex worker",
    rootId: "first",
    provider: "codex",
    accountKey: "default",
  },
  {
    id: "w2",
    name: "Second codex worker",
    rootId: "first",
    provider: "codex",
    accountKey: "default",
  },
  {
    id: "w3",
    name: "Claude worker",
    rootId: "first",
    provider: "claude",
    accountKey: "claude",
  },
] satisfies Agent[];
function Fixture() {
  window.canonicalSettingsFixture.ready = true;
  const [agent, setAgent] = useState<Agent>(initial),
    [mount, setMount] = useState(0),
    [defaults, setDefaults] = useState(false);
  window.canonicalSettingsFixture.setAgent = (value) =>
    flushSync(() => setAgent(value));
  window.canonicalSettingsFixture.agent = agent;
  window.canonicalSettingsFixture.reset = (options) =>
    flushSync(() => {
      setAgent(Object.assign(structuredClone(initial), options?.agent));
      setDefaults(!!options?.defaults);
      setMount((value) => value + 1);
      window.canonicalSettingsFixture.catalogRequests = [];
      window.canonicalSettingsFixture.calls = [];
      window.canonicalSettingsFixture.transfers = [];
      window.canonicalSettingsFixture.transferFail = false;
      window.canonicalSettingsFixture.transferHold = false;
      window.canonicalSettingsFixture.transferReply = null;
      window.canonicalSettingsFixture.pendingTransferProjection = null;
      window.canonicalSettingsFixture.catalogHold = false;
      window.canonicalSettingsFixture.reply = null;
      window.canonicalSettingsFixture.fail = null;
      window.canonicalSettingsFixture.hold = false;
      window.canonicalSettingsFixture.refreshFailure = false;
      window.canonicalSettingsFixture.refreshHold = false;
      window.canonicalSettingsFixture.refreshCount = 0;
      window.canonicalSettingsFixture.release = null;
      window.canonicalSettingsFixture.refreshRelease = null;
      window.canonicalSettingsFixture.transferRelease = null;
      window.canonicalSettingsFixture.serverAccount = null;
    });
  // @ts-expect-error stale fixture: ModelCatalogSource now requires subscribeState.
  const catalogSource: NonNullable<ContextType<typeof ModelCatalogReader>> = {
    scope: "settings-canonical-" + mount,
    read: fixtureModelRead,
  };
  return (
    <MantineProvider>
      <ModelCatalogReader.Provider value={catalogSource}>
        <ExecutionSettings
          key={mount}
          agent={agent}
          accounts={[
            fixtureAccount({
              id: "default",
              label: "",
              email: "first@example.com",
              status: "ready",
            }),
            fixtureAccount({
              id: "second",
              label: "",
              email: "second@example.com",
              status: "ready",
            }),
            fixtureAccount({
              id: "claude",
              label: "Claude",
              provider: "claude",
              status: "ready",
            }),
            fixtureAccount({
              id: "offline",
              label: "Offline",
              status: "error",
            }),
          ]}
          team={team}
          catalog={{ models, loading: false, error: "", retry: () => {} }}
          refresh={async () => {
            window.canonicalSettingsFixture.refreshCount++;
            if (window.canonicalSettingsFixture.refreshHold)
              await new Promise<void>(
                (resolve) =>
                  (window.canonicalSettingsFixture.refreshRelease = resolve),
              );
            if (window.canonicalSettingsFixture.refreshFailure)
              throw new Error("Snapshot unavailable");
            if (window.canonicalSettingsFixture.pendingTransferProjection) {
              window.canonicalSettingsFixture.setAgent(
                window.canonicalSettingsFixture.pendingTransferProjection,
              );
              window.canonicalSettingsFixture.pendingTransferProjection = null;
            }
          }}
        />
        {!defaults && (
          <ExecutionSettings
            key={"permissions:" + mount}
            agent={agent}
            catalog={{ models, loading: false, error: "", retry: () => {} }}
            refresh={async () => {
              window.canonicalSettingsFixture.refreshCount++;
              if (window.canonicalSettingsFixture.refreshHold)
                await new Promise<void>(
                  (resolve) =>
                    (window.canonicalSettingsFixture.refreshRelease = resolve),
                );
              if (window.canonicalSettingsFixture.refreshFailure)
                throw new Error("Snapshot unavailable");
              if (window.canonicalSettingsFixture.pendingTransferProjection) {
                window.canonicalSettingsFixture.setAgent(
                  window.canonicalSettingsFixture.pendingTransferProjection,
                );
                window.canonicalSettingsFixture.pendingTransferProjection =
                  null;
              }
            }}
          />
        )}
      </ModelCatalogReader.Provider>
    </MantineProvider>
  );
}
createRoot(fixtureRoot).render(<Fixture />);

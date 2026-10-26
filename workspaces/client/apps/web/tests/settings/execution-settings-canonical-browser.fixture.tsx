import type { Agent } from "../../src/types.ts";
import type { GetOptions } from "../../src/api.ts";
import type { PostBody, PostResult } from "../../src/apiTypes.ts";
import {
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
      | "reviewDefaults"
      | "accountTransfer"
      | "pendingSettings"
      | "pendingSettingsAccountKey"
      | "isLead"
      | "provider"
      | "yoloMode"
    >
  > & { workerDefaults?: Partial<NonNullable<Agent["workerDefaults"]>> | null };
  defaults?: boolean;
};

declare global {
  interface Window {
    catalogRequests: string[];
    calls: PostBody<"/api/conversation">[];
    transfers: PostBody<"/api/agents/account-transfer">[];
    transferFail: boolean;
    transferHold: boolean;
    transferReply: Partial<PostResult<"/api/agents/account-transfer">> | null;
    pendingTransferProjection: Agent | null;
    reply: Agent | null;
    canonicalFail: string | null;
    hold: boolean;
    refreshFailure: boolean;
    refreshHold: boolean;
    refreshCount: number;
    catalogHold: boolean;
    catalogRelease: (() => void) | null;
    canonicalRelease: (() => void) | null;
    refreshRelease: (() => void) | null;
    transferRelease: (() => void) | null;
    serverAccount: string | null;
    agent: Agent;
    setAgent: (value: Agent) => void;
    canonicalReset: (options?: CanonicalReset) => void;
  }
}
const fixtureRoot = document.getElementById("root");
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
// Partial fixture view: the base fixture omits inherited worker model, effort and fastMode.
function partialAgent(
  value: Omit<Agent, "workerDefaults"> & {
    workerDefaults?: Partial<NonNullable<Agent["workerDefaults"]>> | null;
  },
): Agent {
  return value as Agent;
}
const models = ["gpt-6-astra", "gpt-5.6-sol", "gpt-5.6-luna", "gpt-6-luna"].map(
  (model) => ({
    model,
    supportedReasoningEfforts: ["low", "medium", "high", "max"].map(
      (reasoningEffort) => ({ reasoningEffort }),
    ),
    serviceTiers: [{ id: "priority" }],
    availableAccessPrograms: { cyber: ["standard", "daybreakBlue"] },
  }),
);
window.catalogRequests = [];
window.calls = [];
window.transfers = [];
window.transferFail = false;
window.transferHold = false;
window.transferReply = null;
window.pendingTransferProjection = null;
window.reply = null;
window.canonicalFail = null;
window.hold = false;
window.refreshFailure = false;
window.refreshHold = false;
window.refreshCount = 0;
window.calls = [];
window.transfers = [];
window.transferFail = false;
window.transferHold = false;
window.transferReply = null;
window.pendingTransferProjection = null;
window.reply = null;
window.canonicalFail = null;
window.hold = false;
window.refreshFailure = false;
window.refreshHold = false;
window.refreshCount = 0;
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
    true,
  );
  if (String(url).startsWith("/api/models?")) {
    window.catalogRequests.push(url);
    if (window.catalogHold)
      await new Promise<void>((resolve) => (window.catalogRelease = resolve));
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
    window.transfers.push(body);
    if (window.transferHold)
      await new Promise<void>((resolve) => (window.transferRelease = resolve));
    if (window.transferFail) throw new TypeError("Connection lost");
    const result = window.transferReply || {
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
    window.pendingTransferProjection = partialAgent({
      ...window.agent,
      workerDefaults: {
        ...window.agent.workerDefaults,
        accountKey: body.account_key,
      },
    });
    return fixtureResponse(result);
  }
  if (url != "/api/conversation") return nativeFetch(url, requestOptions);
  window.calls.push(fixturePostBody<"/api/conversation">(requestOptions?.body));
  if (
    window.serverAccount &&
    window.calls.at(-1)!.expected_account_key !== window.serverAccount
  )
    return fixtureResponse(
      { error: "The account changed. Select the model again" },
      { status: 409 },
    );
  if (window.hold)
    await new Promise<void>((resolve) => (window.canonicalRelease = resolve));
  if (window.canonicalFail === "network")
    throw new TypeError("Connection lost");
  return fixtureResponse(
    window.canonicalFail ? { error: "Rejected" } : window.reply,
    {
      status: window.canonicalFail ? 409 : 200,
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
  const [agent, setAgent] = useState<Agent>(initial),
    [mount, setMount] = useState(0),
    [defaults, setDefaults] = useState(false);
  window.setAgent = (value) => flushSync(() => setAgent(value));
  window.agent = agent;
  window.canonicalReset = (options) =>
    flushSync(() => {
      setAgent(partialAgent({ ...initial, ...options?.agent }));
      setDefaults(!!options?.defaults);
      setMount((value) => value + 1);
      window.catalogRequests = [];
      window.calls = [];
      window.transfers = [];
      window.transferFail = false;
      window.transferHold = false;
      window.transferReply = null;
      window.pendingTransferProjection = null;
      window.catalogHold = false;
      window.reply = null;
      window.canonicalFail = null;
      window.hold = false;
      window.refreshFailure = false;
      window.refreshHold = false;
      window.refreshCount = 0;
      window.canonicalRelease = null;
      window.refreshRelease = null;
      window.transferRelease = null;
      window.serverAccount = null;
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
            window.refreshCount++;
            if (window.refreshHold)
              await new Promise<void>(
                (resolve) => (window.refreshRelease = resolve),
              );
            if (window.refreshFailure) throw new Error("Snapshot unavailable");
            if (window.pendingTransferProjection) {
              window.setAgent!(window.pendingTransferProjection);
              window.pendingTransferProjection = null;
            }
          }}
        />
        {!defaults && (
          <ExecutionSettings
            key={"permissions:" + mount}
            agent={agent}
            catalog={{ models, loading: false, error: "", retry: () => {} }}
            refresh={async () => {
              window.refreshCount++;
              if (window.refreshHold)
                await new Promise<void>(
                  (resolve) => (window.refreshRelease = resolve),
                );
              if (window.refreshFailure)
                throw new Error("Snapshot unavailable");
              if (window.pendingTransferProjection) {
                window.setAgent!(window.pendingTransferProjection);
                window.pendingTransferProjection = null;
              }
            }}
          />
        )}
      </ModelCatalogReader.Provider>
    </MantineProvider>
  );
}
createRoot(fixtureRoot!).render(<Fixture />);

import { required } from "./fixture-helpers.ts";
import type { Agent } from "../../src/types.ts";
import type { WorkerModelInfo } from "../../src/components/agents/WorkerModelPicker.tsx";
import type { PostBody } from "../../src/apiTypes.ts";
import {
  fixtureNotReady,
  fixtureFetchInput,
  fixturePostBody,
} from "./fixture-helpers.ts";
export type DaybreakReset = {
  agent?: Partial<
    Pick<
      Agent,
      | "model"
      | "effort"
      | "fastMode"
      | "daybreakEnabled"
      | "status"
      | "provider"
      | "pendingSettingsAccountKey"
      | "pendingSettings"
      | "workerDefaults"
    >
  >;
  models?: WorkerModelInfo[];
  defaults?: boolean;
};
type DaybreakHarness = {
  ready: boolean;
  required: typeof required;
  calls: PostBody<"/api/conversation">[];
  fail: "network" | "rejected" | null;
  reply: Agent | null;
  refreshCount: number;
  retries: number;
  models: WorkerModelInfo[];
  agent: Agent;
  setAgent: (agent: Agent) => void;
  setModels: (models: WorkerModelInfo[]) => void;
  reset: (options?: DaybreakReset) => void;
  hold: boolean;
  release: () => void;
};
declare global {
  interface Window {
    daybreakFixture: DaybreakHarness;
  }
}
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
import { createRoot } from "react-dom/client";
import { flushSync } from "react-dom";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import { ExecutionSettings } from "../../src/components/agents/ExecutionSettings.tsx";
const initial: Agent = {
  id: "first",
  accountKey: "default",
  name: "First",
  source: "managed",
  model: "gpt-5.6-sol",
  effort: "high",
  fastMode: true,
  daybreakEnabled: false,
  isLead: true,
  status: "idle",
  created: 1,
  yoloMode: false,
  nextTurnSettingsSupported: true,
};
const row = (
  model: string,
  cyber: string[] | undefined,
  extra: Partial<WorkerModelInfo> = {},
): WorkerModelInfo => ({
  model,
  displayName: model,
  supportedReasoningEfforts: ["low", "high", "max"].map((reasoningEffort) => ({
    reasoningEffort,
  })),
  serviceTiers: [{ id: "priority" }],
  ...(cyber === undefined ? {} : { availableAccessPrograms: { cyber } }),
  ...extra,
});
const initialModels = [
  row("gpt-6-astra", ["standard"]),
  row("gpt-5.6-sol", ["standard", "daybreakBlue"], { isDefault: true }),
  row("gpt-5.6-terra", ["standard", "daybreakRed"]),
  row("gpt-5.6-luna", undefined),
  row("daybreak-only", ["daybreakBlue"], {
    serviceTiers: [],
    supportedReasoningEfforts: [{ reasoningEffort: "low" }],
  }),
  row("gpt-daybreak-blue-latest", ["daybreakBlue"]),
  row("gpt-daybreak-red-latest", ["daybreakRed"]),
];
window.daybreakFixture = {
  ready: false,
  required,
  calls: [],
  fail: null,
  reply: null,
  refreshCount: 0,
  retries: 0,
  models: initialModels,
  agent: initial,
  setAgent: fixtureNotReady,
  setModels: fixtureNotReady,
  reset: fixtureNotReady,
  hold: false,
  release: fixtureNotReady,
};
window.daybreakFixture.fail = null;
window.daybreakFixture.reply = null;
window.daybreakFixture.refreshCount = 0;
window.daybreakFixture.retries = 0;
const nativeFetch = window.fetch;
window.fetch = async (input, options) => {
  const { url, options: requestOptions } = await fixtureFetchInput(
    input,
    options,
  );
  if (url.startsWith("/api/models"))
    return new Response(
      JSON.stringify({ data: window.daybreakFixture.models }),
      {
        headers: fixtureSchemaHeaders,
      },
    );
  if (url != "/api/conversation") return nativeFetch(url, requestOptions);
  const request = fixturePostBody<"/api/conversation">(requestOptions?.body);
  window.daybreakFixture.calls.push(request);
  if (window.daybreakFixture.hold)
    await new Promise<void>(
      (resolve) => (window.daybreakFixture.release = resolve),
    );
  if (window.daybreakFixture.fail === "network")
    throw new TypeError("Connection lost");
  if (window.daybreakFixture.fail === "rejected")
    return new Response(JSON.stringify({ error: "Mode not supported" }), {
      status: 422,
      headers: fixtureSchemaHeaders,
    });
  const values = request.worker_defaults || request;
  const settings = {
    model: values.model,
    effort: values.effort,
    fastMode: values.fast_mode,
    daybreakEnabled: values.daybreak_enabled,
  };
  const canonical = window.daybreakFixture.reply || {
    ...window.daybreakFixture.agent,
    ...(request.worker_defaults
      ? { workerDefaults: settings }
      : request.next_turn
        ? {
            pendingSettings: settings,
            pendingSettingsAccountKey: window.daybreakFixture.agent.accountKey,
          }
        : settings),
  };
  return new Response(JSON.stringify(canonical), {
    status: 200,
    headers: fixtureSchemaHeaders,
  });
};
function Fixture() {
  window.daybreakFixture.ready = true;
  const [agent, setAgent] = useState<Agent>(initial),
    [models, setModels] = useState(initialModels),
    [mount, setMount] = useState(0),
    [defaults, setDefaults] = useState(false);
  window.daybreakFixture.models = models;
  window.daybreakFixture.agent = agent;
  window.daybreakFixture.setAgent = (value) => flushSync(() => setAgent(value));
  window.daybreakFixture.setModels = (value) =>
    flushSync(() => setModels(value));
  window.daybreakFixture.reset = (options) =>
    flushSync(() => {
      setAgent(Object.assign(structuredClone(initial), options?.agent));
      setModels(options?.models || initialModels);
      setDefaults(!!options?.defaults);
      setMount((value) => value + 1);
      window.daybreakFixture.calls = [];
      window.daybreakFixture.fail = null;
      window.daybreakFixture.reply = null;
      window.daybreakFixture.hold = false;
      window.daybreakFixture.refreshCount = 0;
      window.daybreakFixture.retries = 0;
    });
  return (
    <MantineProvider>
      <ExecutionSettings
        key={mount}
        agent={agent}
        initialRole={defaults ? "worker" : "orchestrator"}
        catalog={{
          models,
          loading: false,
          error: "",
          retry: () => window.daybreakFixture.retries++,
        }}
        refresh={async () => {
          window.daybreakFixture.refreshCount++;
        }}
      />
    </MantineProvider>
  );
}
createRoot(fixtureRoot).render(<Fixture />);

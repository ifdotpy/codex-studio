import type { Agent } from "../../src/types.ts";
import type { WorkerModelInfo } from "../../src/components/agents/WorkerModelPicker.tsx";
import type { PostBody } from "../../src/apiTypes.ts";
import { fixtureFetchInput, fixturePostBody } from "./fixture-helpers.ts";
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

declare global {
  interface Window {
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
window.calls = [];
window.fail = null;
window.reply = null;
window.refreshCount = 0;
window.retries = 0;
window.fail = null;
window.reply = null;
window.refreshCount = 0;
window.retries = 0;
const nativeFetch = window.fetch;
window.fetch = async (input, options) => {
  const { url, options: requestOptions } = await fixtureFetchInput(
    input,
    options,
    true,
  );
  if (url.startsWith("/api/models"))
    return new Response(JSON.stringify({ data: window.models }), {
      headers: fixtureSchemaHeaders,
    });
  if (url != "/api/conversation") return nativeFetch(url, requestOptions);
  const request = fixturePostBody<"/api/conversation">(requestOptions?.body);
  window.calls.push(request);
  if (window.hold)
    await new Promise<void>((resolve) => (window.release = resolve));
  if (window.fail === "network") throw new TypeError("Connection lost");
  if (window.fail === "rejected")
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
  const canonical = window.reply || {
    ...window.agent,
    ...(request.worker_defaults
      ? { workerDefaults: settings }
      : request.next_turn
        ? {
            pendingSettings: settings,
            pendingSettingsAccountKey: window.agent.accountKey,
          }
        : settings),
  };
  return new Response(JSON.stringify(canonical), {
    status: 200,
    headers: fixtureSchemaHeaders,
  });
};
function Fixture() {
  const [agent, setAgent] = useState<Agent>(initial),
    [models, setModels] = useState(initialModels),
    [mount, setMount] = useState(0),
    [defaults, setDefaults] = useState(false);
  window.models = models;
  window.agent = agent;
  window.setAgent = (value) => flushSync(() => setAgent(value));
  window.setModels = (value) => flushSync(() => setModels(value));
  window.reset = (options) =>
    flushSync(() => {
      setAgent({ ...initial, ...options?.agent });
      setModels(options?.models || initialModels);
      setDefaults(!!options?.defaults);
      setMount((value) => value + 1);
      window.calls = [];
      window.fail = null;
      window.reply = null;
      window.hold = false;
      window.refreshCount = 0;
      window.retries = 0;
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
          retry: () => window.retries++,
        }}
        refresh={async () => {
          window.refreshCount++;
        }}
      />
    </MantineProvider>
  );
}
createRoot(fixtureRoot!).render(<Fixture />);

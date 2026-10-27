import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { expect, expectTypeOf, test, vi } from "vitest";
import { confirmedTransfer, ExecutionSettings } from "./ExecutionSettings";
import type { PostResult } from "../../apiTypes.ts";
import { TransferStatus } from "../../generated/api";
import type { Agent } from "../../types";

const capability = vi.hoisted(() => ({ priority: false }));

type TransferResponse = PostResult<"/api/agents/account-transfer">;
const transferResponse = {
  id: "operation",
  leadId: "lead",
  targetAccountKey: "target",
  scope: "subagents",
  status: "pending",
  created: 1,
  members: {},
  requests: {
    extension: {
      leadId: "lead",
      targetAccountKey: "target",
      scope: "subagents",
    },
  },
} satisfies TransferResponse;

test("transfer confirmation preserves the generated response and receipt types", () => {
  expectTypeOf<
    (typeof TransferStatus)[keyof typeof TransferStatus]
  >().toEqualTypeOf<TransferResponse["status"]>();
  expectTypeOf(confirmedTransfer)
    .parameter(0)
    .toEqualTypeOf<TransferResponse>();
  expectTypeOf(confirmedTransfer).returns.toEqualTypeOf<TransferResponse>();
  expectTypeOf(confirmedTransfer)
    .parameter(4)
    .toEqualTypeOf<TransferResponse["scope"]>();
  const confirmed = confirmedTransfer(
    transferResponse,
    "extension",
    "lead",
    "target",
    "subagents",
  );
  expect(confirmed).toBe(transferResponse);
  expect(confirmed.requests?.extension).toBe(
    transferResponse.requests.extension,
  );
});

for (const requestId of ["operation", "extension"]) {
  test(`transfer confirmation accepts correlated ${requestId} identity`, () => {
    expect(
      confirmedTransfer(
        transferResponse,
        requestId,
        "lead",
        "target",
        "subagents",
      ).id,
    ).toBe("operation");
  });
}

for (const patch of [
  { leadId: "another-lead" },
  { targetAccountKey: "another-account" },
  { scope: "team" },
  { requests: null },
  {
    requests: {
      extension: {
        ...transferResponse.requests.extension,
        leadId: "another-lead",
      },
    },
  },
  {
    requests: {
      extension: {
        ...transferResponse.requests.extension,
        targetAccountKey: "another-account",
      },
    },
  },
  {
    requests: {
      extension: { ...transferResponse.requests.extension, scope: "team" },
    },
  },
] satisfies Partial<TransferResponse>[]) {
  test(`transfer confirmation rejects mismatched identity ${JSON.stringify(patch)}`, () => {
    expect(() =>
      confirmedTransfer(
        { ...transferResponse, ...patch },
        "extension",
        "lead",
        "target",
        "subagents",
      ),
    ).toThrow("The server did not confirm this account transfer.");
  });
}

test("transfer confirmation rejects malformed runtime ingress", () => {
  for (const value of [
    null,
    [],
    "operation",
    { ...transferResponse, status: "unsupported" },
    { ...transferResponse, id: 42 },
    { ...transferResponse, requests: { extension: null } },
  ]) {
    expect(() =>
      // @ts-expect-error Deliberately malformed transport data still needs runtime rejection.
      confirmedTransfer(value, "extension", "lead", "target", "subagents"),
    ).toThrow("The server did not confirm this account transfer.");
  }
});
vi.mock("./WorkerModelPicker", async (load) => ({
  ...(await load<typeof import("./WorkerModelPicker")>()),
  useWorkerModels: () => ({
    models: [
      {
        model: "gpt-6-astra",
        serviceTiers: capability.priority ? [{ id: "priority" }] : [],
        supportedReasoningEfforts: [],
      },
    ],
    loading: false,
    error: "",
    retry: () => {},
  }),
}));

vi.mock("../models/modelWhitelist", async (load) => ({
  ...(await load<typeof import("../models/modelWhitelist")>()),
  useModelWhitelist: () => null,
}));

for (const initialRole of ["orchestrator", "worker", "review"] as const) {
  for (const priority of [true, false]) {
    test(`${initialRole} Fast mode follows the selected model capability (${priority})`, () => {
      capability.priority = priority;
      const agent: Agent = {
        id: "fast-picker",
        isLead: true,
        source: "managed",
        status: "idle",
        model: "gpt-6-astra",
        workerDefaults: { model: "gpt-6-astra", effort: null, fastMode: false },
        reviewDefaults: { model: "gpt-6-astra", effort: null },
      };
      const markup = renderToStaticMarkup(
        createElement(
          MantineProvider,
          null,
          createElement(ExecutionSettings, {
            presentation: "panel",
            initialRole,
            agent,
            catalog: {
              models: [
                {
                  model: "gpt-6-astra",
                  serviceTiers: priority ? [{ id: "priority" }] : [],
                  supportedReasoningEfforts: [],
                },
              ],
              loading: false,
              error: "",
              retry: () => {},
            },
            refresh: async () => {},
          }),
        ),
      );
      expect(markup.includes('aria-label="Fast mode"')).toBe(priority);
      if (initialRole === "review") {
        expect(markup).not.toContain('aria-label="Daybreak"');
        expect(markup).toContain("review tasks use Workers");
      }
    });
  }
}

for (const isLead of [true, false]) {
  test(`the ${isLead ? "lead" : "worker"} panel always renders every role`, () => {
    const markup = renderToStaticMarkup(
      createElement(
        MantineProvider,
        null,
        createElement(ExecutionSettings, {
          presentation: "panel",
          settingsRow: true,
          agent: {
            id: "picker-test",
            name: "Test",
            isLead,
            source: "managed",
            model: "gpt-6-astra",
            status: "idle",
          },
          catalog: { models: [], loading: false, error: "", retry: () => {} },
          refresh: async () => {},
        }),
      ),
    );
    for (const label of ["Lead", "Workers", "Review"]) {
      expect(markup).toContain(`>${label}</span></label>`);
    }
    expect(markup.match(/aria-label="Agent role"/g)).toHaveLength(1);
  });
}

for (const initialRole of ["orchestrator", "review"] as const) {
  test(`a worker without its lead keeps ${initialRole} visible as unavailable`, () => {
    const markup = renderToStaticMarkup(
      createElement(
        MantineProvider,
        null,
        createElement(ExecutionSettings, {
          presentation: "panel",
          initialRole,
          agent: {
            id: "worker",
            name: "Worker",
            isLead: false,
            source: "managed",
            model: "gpt-6-luna",
            status: "idle",
          },
          catalog: { models: [], loading: false, error: "", retry: () => {} },
          refresh: async () => {},
        }),
      ),
    );
    expect(markup).toContain(
      "Managed by the lead. Its settings are unavailable in this chat.",
    );
    expect(markup).toContain("Inherited settings unavailable");
    expect(markup).toContain('aria-disabled="true"');
    expect(markup).not.toContain('data-value="gpt-6-luna"');
    for (const label of ["Lead", "Workers", "Review"])
      expect(markup).toContain(`>${label}</span></label>`);
  });
}

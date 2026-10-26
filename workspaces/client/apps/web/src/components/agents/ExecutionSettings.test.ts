import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { expect, test, vi } from "vitest";
import { ExecutionSettings } from "./ExecutionSettings";

const scope = vi.hoisted(() => ({
  owner: Object.freeze({
    id: "settings-fixture",
    origin: "https://settings.example.test",
    credentialId: "settings-fixture-credential",
  }),
  api: {
    saved: (_key: string, fallback: unknown) => fallback,
    save: vi.fn(),
    get: vi.fn(),
    post: vi.fn(),
  },
  sync: { subscribeStateProjection: vi.fn(() => () => {}) },
}));

// Layout fixtures use the same explicit owner/API/sync seams as a screen.
// They must not acquire the browser's local-storage fallback during SSR.
vi.mock("../../servers/ServerServicesContext", async (load) => ({
  ...(await load<typeof import("../../servers/ServerServicesContext")>()),
  useServerOwner: () => scope.owner,
  useServerApi: () => scope.api,
  useServerSync: () => scope.sync,
}));

vi.mock("../models/modelWhitelist", async (load) => ({
  ...(await load<typeof import("../models/modelWhitelist")>()),
  useModelWhitelist: () => null,
}));

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

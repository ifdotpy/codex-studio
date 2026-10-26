import React from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import VaultSetup from "../../src/components/sidebar/VaultSetup.tsx";
import ClaudeSignIn from "../../src/components/ClaudeSignIn.tsx";
import { createServerServices } from "../../src/servers/serverServices.ts";
import { ServerServicesProvider } from "../../src/servers/ServerServicesContext.tsx";
import type { ApiPostPath, PostBody, PostResult } from "../../src/apiTypes.ts";
import type {
  ResourceProgress,
  ResourceRef,
  ResourceVersion,
} from "../../src/sync/subscriptions/contracts.ts";
import { fixtureElement } from "./browser-helpers.ts";

const reads = { facts: 0, auth: 0, jobs: 0 };
const selectors: ResourceRef[] = [];
const listeners = new Map<string, (version?: ResourceVersion) => void>();
const defaults = createServerServices({
  id: "local",
  label: "Fixture",
  origin: location.origin,
});
const resources = {
  ...defaults.resources,
  watchResourceChanges(
    resource: ResourceRef,
    accept: (version?: ResourceVersion) => void,
  ) {
    const key = JSON.stringify(resource);
    listeners.set(key, accept);
    selectors.push(resource);
    return () => {
      listeners.delete(key);
    };
  },
  watchResourceConnection(
    accept: Parameters<typeof defaults.resources.watchResourceConnection>[0],
  ) {
    accept("live", "open", {});
    return () => {};
  },
};
// The fixture writes this exact saved login before the real component reads it.
defaults.api.save("claude-sign-in:fixture:claude-one", "login-one");
const api = {
  ...defaults.api,
  get: async () => {
    reads.auth++;
    throw new Error("unexpected auth read");
  },
  save: () => {},
  stableRequestId: () => "",
  clearStableRequestId: () => {},
};
const flow = new URL(location.href).searchParams.get("flow");
// Responses are made by this fixture or the spec's typed route handler.
function fixtureReply<P extends ApiPostPath>(value: unknown): PostResult<P> {
  return value as PostResult<P>;
}
const backend = {
  post: async <P extends ApiPostPath>(
    path: P,
    body: PostBody<P>,
  ): Promise<PostResult<P>> => {
    if (path !== "/api/vault/setup/facts") {
      reads.jobs++;
      const response = await fetch(path, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(body),
      });
      return fixtureReply<P>(await response.json());
    }
    reads.facts++;
    return fixtureReply<P>({
      gatewayProfile: null,
      prefix: "project-one",
      keys: flow === "vault-job",
      site: "https://desk.example",
      region: "eu-west-1",
      profiles:
        flow === "vault-job" ? [{ name: "fixture", signedIn: true }] : [],
    } satisfies PostResult<"/api/vault/setup/facts">);
  },
};
const services = {
  ...defaults,
  api,
  resources,
  fileDialogs: {
    ...defaults.fileDialogs,
    showVaultKeys: async () => {},
    canShowVaultKeys: () => true,
  },
};
const progress = (progress: ResourceProgress) =>
  listeners.get(JSON.stringify(progress.resource))?.({
    epoch: "fixture",
    revision: 1,
    progress,
  });
window.transientProgressFixture = { reads, selectors, progress, ready: false };
createRoot(fixtureElement("root")).render(
  <MantineProvider>
    <ServerServicesProvider services={services}>
      {flow !== "auth" ? (
        <VaultSetup
          entry={{ vault: { projectId: "p1", state: "ok" } }}
          name="Project"
          backend={backend}
          onClose={() => {}}
          onDone={() => {
            window.transientProgressFixture.ready = true;
          }}
        />
      ) : (
        <ClaudeSignIn
          account={{
            id: "claude-one",
            label: "Claude",
            provider: "claude",
            status: "signedOut",
            home: "",
            source: "fixture",
          }}
          scope="fixture"
          onClose={() => {}}
          onReady={() => {
            window.transientProgressFixture.ready = true;
          }}
        />
      )}
    </ServerServicesProvider>
  </MantineProvider>,
);
declare global {
  interface Window {
    transientProgressFixture: {
      reads: typeof reads;
      selectors: typeof selectors;
      progress: typeof progress;
      ready: boolean;
    };
  }
}

import React from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import VaultSetup from "../../src/components/sidebar/VaultSetup.tsx";
import ClaudeSignIn from "../../src/components/ClaudeSignIn.tsx";
import type { ServerServices } from "../../src/servers/serverServices.ts";
import { ServerServicesProvider } from "../../src/servers/ServerServicesContext.tsx";
import type { ApiPostPath, PostInput, PostResult } from "../../src/apiTypes.ts";
import type {
  ResourceProgress,
  ResourceRef,
  ResourceVersion,
} from "../../src/sync/subscriptions/contracts.ts";
import { fixtureElement } from "./browser-helpers.ts";

const reads = { facts: 0, auth: 0, jobs: 0 };
window.reads = reads;
const selectors: ResourceRef[] = (window.selectors = []);
const listeners = new Map<string, (version?: ResourceVersion) => void>();
const resources = {
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
    accept: (
      state: Parameters<
        Parameters<ServerServices["resources"]["watchResourceConnection"]>[0]
      >[0],
    ) => void,
  ) {
    accept("live");
    return () => {};
  },
};
const api = {
  get: async () => {
    reads.auth++;
    throw new Error("unexpected auth read");
  },
  saved: (key: string, fallback: unknown) =>
    key === "claude-sign-in:fixture:claude-one" ? "login-one" : fallback,
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
    body: PostInput<P>,
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
      prefix: "project-one",
      keys: flow === "vault-job",
      site: "https://desk.example",
      region: "eu-west-1",
      profiles:
        flow === "vault-job" ? [{ name: "fixture", signedIn: true }] : [],
    });
  },
};
// This one-off fixture omits unused service adapters and API methods.
const services: Partial<Record<keyof ServerServices, unknown>> = {
  owner: { id: "local", origin: location.origin },
  api,
  resources,
  fileDialogs: {
    showVaultKeys: async () => {},
    canShowVaultKeys: () => true,
  },
};
// These one-off fixtures omit the unused vault state and account home/source fields.
const entry = { vault: { projectId: "p1" } } as React.ComponentProps<
  typeof VaultSetup
>["entry"];
const account = {
  id: "claude-one",
  label: "Claude",
  provider: "claude",
  status: "signedOut",
} as React.ComponentProps<typeof ClaudeSignIn>["account"];
// The spec supplies the correlated resource/value pair at this fixture boundary.
const progress = (
  resource: ResourceProgress["resource"],
  value: ResourceProgress["value"],
) =>
  listeners.get(JSON.stringify(resource))?.({
    epoch: "fixture",
    revision: 1,
    progress: { resource, value } as ResourceProgress,
  });
window.progress = progress;
createRoot(fixtureElement("root")).render(
  <MantineProvider>
    <ServerServicesProvider services={services as ServerServices}>
      {flow !== "auth" ? (
        <VaultSetup
          entry={entry}
          name="Project"
          backend={backend}
          onClose={() => {}}
          onDone={() => {
            window.ready = true;
          }}
        />
      ) : (
        <ClaudeSignIn
          account={account}
          scope="fixture"
          onClose={() => {}}
          onReady={() => {
            window.ready = true;
          }}
        />
      )}
    </ServerServicesProvider>
  </MantineProvider>,
);
declare global {
  interface Window {
    reads: typeof reads;
    selectors: typeof selectors;
    progress: typeof progress;
    ready?: boolean;
  }
}

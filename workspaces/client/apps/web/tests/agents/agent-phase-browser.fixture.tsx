import type { Agent } from "../../src/types.ts";
import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { useMessages } from "../../src/hooks.ts";
import AgentPhase from "../../src/components/agents/AgentPhase.tsx";
import { ServerServicesProvider } from "../../src/servers/ServerServicesContext.tsx";
import { createServerServices } from "../../src/servers/serverServices.ts";
import { createSubscriptions } from "../../src/sync/subscriptions/module.ts";
import { createResourceEvents } from "../../src/sync/resourceEvents.ts";
import type {
  ResourceEventsOptions,
  ResourceRef,
} from "../../src/sync/resourceEvents.ts";
import type { ResourceData } from "../../src/sync/subscriptions/contracts.ts";
import type { ServerEventSource } from "../../src/servers/eventSource.ts";
import { API_SCHEMA_HASH } from "../../src/generated/apiSchema.ts";
import { SYNC_PROTOCOL_VERSION } from "../../src/generated/api.ts";
import { SYNC_TRANSCRIPT_INITIAL_TAIL_ITEMS } from "../../src/generated/sharedValues.ts";
import { fixturePostBody, fixturePostResult } from "../misc-fixture.ts";

type TranscriptRef = Extract<ResourceRef, { kind: "transcript" }>;
type Transcript = ResourceData<TranscriptRef>;
type ConnectionListener = Parameters<
  ReturnType<typeof createResourceEvents>["watchResourceConnection"]
>[0];
declare global {
  interface Window {
    chatConnectionFixture: {
      reads: unknown[][];
      renders: Record<string, number>;
      setConnection: (
        value: "offline" | "connecting" | "live",
      ) => void | Promise<void>;
      connectionListeners: () => number;
      repeatLive: () => void;
      phase: (id: string, phase: string) => void;
      selectChat: (id: string) => void;
      selectServer: (other: boolean) => void;
    };
  }
}
window.chatConnectionFixture = {
  reads: [],
  renders: { first: 0, second: 0 },
  setConnection: () => {},
  connectionListeners: () => 0,
  repeatLive: () => {},
  phase: () => {},
  selectChat: () => {},
  selectServer: () => {},
};
const harness = window.chatConnectionFixture;
const refs = new Map<string, TranscriptRef>(
  ["first", "second"].map((agentId) => [
    agentId,
    { kind: "transcript", agentId, tail: SYNC_TRANSCRIPT_INITIAL_TAIL_ITEMS },
  ]),
);
const payloads = new Map<string, Transcript>(
  ["first", "second"].map((id) => [
    id,
    {
      items: [],
      truncated: false,
      agent: { id, status: "running", activity: { phase: "writing" } },
    },
  ]),
);
// The fixture owns the subscription payload paired with each transcript ref.
function fixtureData<R extends ResourceRef>(value: unknown): ResourceData<R> {
  return value as ResourceData<R>;
}
function scope(ownerId: string, hold = false) {
  const connections = new Set<ConnectionListener>();
  let revision = 0;
  let retryReady: (() => void) | undefined;
  class Source extends EventTarget implements ServerEventSource {
    closed = false;
    resources: ResourceRef[];
    onopen: ServerEventSource["onopen"] = null;
    onerror: ServerEventSource["onerror"] = null;
    constructor(url: string) {
      super();
      // createResourceEvents writes these generated refs into the same-program URL.
      this.resources = JSON.parse(
        new URL(url, location.origin).searchParams.get("resources") ?? "[]",
      ) as ResourceRef[];
    }
    emit(name: string, value: unknown) {
      this.dispatchEvent(
        new MessageEvent(name, { data: JSON.stringify(value) }),
      );
    }
    close() {
      this.closed = true;
    }
    baseline() {
      this.emit("resources", {
        protocol: SYNC_PROTOCOL_VERSION,
        workspaceId: ownerId,
        epoch: "fixture-epoch",
        revision: ++revision,
        reason: "initial",
        resources: this.resources,
        resourceVersions: this.resources.map((resource) => ({
          resource,
          revision,
          cursor: { workspaceId: ownerId, seq: revision },
          replyKind: "delta",
        })),
      });
    }
  }
  const sources: Source[] = [];
  const latest = () => {
    const source = sources.at(-1);
    if (!source) throw new Error("Missing connection source");
    return source;
  };
  const options: ResourceEventsOptions = {
    api: {
      get: (...args: unknown[]) => {
        harness.reads.push(args);
        throw new Error("Unexpected transport read");
      },
      post: async (path, body) => {
        if (path !== "/api/sync/subscriptions")
          throw new Error(`Unexpected post: ${path}`);
        const update = fixturePostBody<"/api/sync/subscriptions">(body);
        const source = latest();
        source.resources = [
          ...source.resources.filter(
            (ref) =>
              !update.remove.some(
                (removed) => JSON.stringify(removed) === JSON.stringify(ref),
              ),
          ),
          ...update.add.map((item) => item.resource),
        ];
        queueMicrotask(() => {
          source.emit("subscriptions", { gen: update.gen });
          if (!hold) source.baseline();
        });
        return fixturePostResult<typeof path>({ gen: update.gen });
      },
      isApiSchemaMismatch: () => false,
      onApiSchemaMismatch: () => () => {},
      matchingApiSchemaResponseGeneration: () => 0,
      clearSchemaUpdateAttemptAfterMatch: () => {},
      markApiSchemaMismatch: () => {
        throw new Error("Unexpected schema mismatch");
      },
    },
    sync: {
      syncDatabase: async () => ({ workspaceId: ownerId }),
      persistResourceEntityChanges: async () => {},
      readSubscriptionCursor: async () => undefined,
    },
    tokenRates: {
      configureTokenRateStream: () => {},
      receiveResourceTokenRates: () => true,
      getWorkspaceTokenRates: () => ({ records: [] }),
    },
    storageName: (name) => ownerId + ":" + name,
    openEvents: (url) => {
      const source = new Source(url);
      sources.push(source);
      queueMicrotask(() => {
        source.onopen?.(new Event("open"));
        source.emit("api-schema", { hash: API_SCHEMA_HASH });
        source.emit("session", { sessionId: ownerId + sources.length });
        if (!hold) source.baseline();
        retryReady?.();
        retryReady = undefined;
      });
      return source;
    },
  };
  const transport = createResourceEvents(options);
  const resources = createSubscriptions(
    {
      ...transport,
      watchResourceConnection: (accept: ConnectionListener) => {
        connections.add(accept);
        const stop = transport.watchResourceConnection(accept);
        return () => {
          connections.delete(accept);
          stop();
        };
      },
    },
    (ref, accept) => {
      if (ref.kind !== "transcript") throw new Error("Unexpected subscription");
      accept(fixtureData<typeof ref>(payloads.get(ref.agentId)));
      return () => {};
    },
  );
  const defaults = createServerServices({
    id: "local",
    label: "Fixture",
    origin: location.origin,
  });
  const services = {
    ...defaults,
    owner: { id: ownerId, label: "Fixture", origin: location.origin },
    resources: { ...defaults.resources, ...resources },
    api: {
      ...defaults.api,
      syncGet: (...args: unknown[]) => {
        harness.reads.push(args);
        throw new Error("Unexpected read");
      },
    },
    transcripts: { ...defaults.transcripts, peekTranscript: () => undefined },
  };
  return {
    resources,
    connections,
    services,
    setConnection(value: "offline" | "connecting" | "live") {
      if (value === "offline") {
        hold = true;
        latest().onerror?.(new Event("error"));
      } else if (value === "connecting") {
        if (latest().closed)
          return new Promise<void>((resolve) => {
            retryReady = resolve;
          });
      } else {
        hold = false;
        latest().baseline();
      }
    },
  };
}
const firstScope = scope("fixture"),
  otherScope = scope("other-server", true);
harness.setConnection = (value) => firstScope.setConnection(value);
harness.connectionListeners = () => firstScope.connections.size;
harness.repeatLive = () => {
  for (const accept of firstScope.connections) accept("live", "open", {});
};
harness.phase = (id, phase) => {
  const old = payloads.get(id),
    ref = refs.get(id);
  if (!old?.agent || !ref) throw new Error("Missing transcript fixture");
  const next = { ...old, agent: { ...old.agent, activity: { phase } } };
  payloads.set(id, next);
  firstScope.resources.publish(ref, next);
};
function liveAgentView(
  agent: Pick<Agent, "id" | "source" | "nativeStatus"> &
    NonNullable<ReturnType<typeof useMessages>["liveAgent"]>,
): Agent {
  // Mirrors Conversation.tsx's existing cast: the generated context-usage views describe
  // the same stored value; unifying their wire contracts is a post-migration follow-up.
  return agent as Agent;
}
function Chat({ id, visible }: { id: string; visible: boolean }) {
  harness.renders[id] = (harness.renders[id] ?? 0) + 1;
  const messages = useMessages(
    id,
    "agent",
    true,
    "fixture",
    "fixture",
    visible,
  );
  const base = {
    id,
    source: "managed",
    status: "running",
    nativeStatus: { phase: "retrying", message: "Provider is reconnecting" },
  } satisfies Agent;
  const agent = liveAgentView({ ...base, ...messages.liveAgent });
  return (
    <section
      hidden={!visible}
      data-chat={id}
      data-connection={messages.connection}
      data-loaded={messages.loaded}
    >
      <AgentPhase agent={agent} connection={messages.connection} />
    </section>
  );
}
function Harness() {
  const [selected, select] = useState("first");
  const [other, selectServer] = useState(false);
  harness.selectChat = select;
  harness.selectServer = selectServer;
  return (
    <ServerServicesProvider
      services={other ? otherScope.services : firstScope.services}
    >
      {["first", "second"].map((id) => (
        <Chat key={id} id={id} visible={id === selected} />
      ))}
    </ServerServicesProvider>
  );
}
const root = document.getElementById("root");
if (!root) throw new Error("Missing connection fixture root");
createRoot(root).render(<Harness />);

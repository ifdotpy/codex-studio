import type { Agent } from "../../src/types.ts";
import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { useMessages } from "../../src/hooks.ts";
import AgentPhase from "../../src/components/agents/AgentPhase.tsx";
import { ServerServicesProvider } from "../../src/servers/ServerServicesContext.tsx";
import type { ServerServices } from "../../src/servers/serverServices.ts";
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
  }
}
window.reads = [];
window.renders = { first: 0, second: 0 };
window.connectionListeners = () => 0;

const refs: Record<string, TranscriptRef> = Object.fromEntries(
  ["first", "second"].map((agentId) => [
    agentId,
    { kind: "transcript", agentId, tail: SYNC_TRANSCRIPT_INITIAL_TAIL_ITEMS },
  ]),
);
const payloads: Record<
  string,
  Omit<Transcript, "truncated">
> = Object.fromEntries(
  ["first", "second"].map((id) => [
    id,
    {
      items: [],
      agent: { id, status: "running", activity: { phase: "writing" } },
    },
  ]),
);
// The fixture owns the subscription payload paired with each transcript ref.
function fixtureData<R extends ResourceRef>(value: unknown): ResourceData<R> {
  return value as ResourceData<R>;
}
// Base event double invokes function listeners with only the serialized data field.
function partialEventListener(value: unknown) {
  return value as (event: Pick<MessageEvent, "data">) => void;
}
// The one-off base services omit unrelated capabilities and the owner label.
function partialServices(value: unknown): ServerServices {
  return value as ServerServices;
}
function scope(ownerId: string, hold = false) {
  const connections = new Set<ConnectionListener>();
  let revision = 0;
  let retryReady: (() => void) | undefined;
  class Source implements ServerEventSource {
    listeners = new Map<string, (event: Pick<MessageEvent, "data">) => void>();
    closed = false;
    resources: ResourceRef[];
    declare onopen: (() => void) | null;
    declare onerror: (() => void) | null;
    constructor(url: string) {
      // createResourceEvents writes these generated refs into the same-program URL.
      this.resources = JSON.parse(
        new URL(url, location.origin).searchParams.get("resources")!,
      ) as ResourceRef[];
    }
    addEventListener(
      name: string,
      accept: EventListenerOrEventListenerObject | null,
    ) {
      this.listeners.set(name, partialEventListener(accept));
    }
    emit(name: string, value: unknown) {
      this.listeners.get(name)?.({ data: JSON.stringify(value) });
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
  const options: ResourceEventsOptions = {
    api: {
      get: (...args: unknown[]) => {
        window.reads.push(args);
        throw new Error("Unexpected transport read");
      },
      post: async (path, body) => {
        const update = fixturePostBody<"/api/sync/subscriptions">(body);
        const source = sources.at(-1)!;
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
        source.onopen?.();
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
      accept(fixtureData<typeof ref>(payloads[(ref as TranscriptRef).agentId]));
      return () => {};
    },
  );
  // Base connection fixture omits unrelated server services, API methods and owner label.
  const services = partialServices({
    owner: { id: ownerId, origin: location.origin },
    resources,
    api: {
      syncGet: (...args: unknown[]) => {
        window.reads.push(args);
        throw new Error("Unexpected read");
      },
    },
    transcripts: { peekTranscript: () => undefined },
  });
  return {
    resources,
    connections,
    services,
    setConnection(value: "offline" | "connecting" | "live") {
      if (value === "offline") {
        hold = true;
        sources.at(-1)!.onerror?.();
      } else if (value === "connecting") {
        if (sources.at(-1)!.closed)
          return new Promise<void>((resolve) => {
            retryReady = resolve;
          });
      } else if (value === "live") {
        hold = false;
        sources.at(-1)!.baseline();
      }
    },
  };
}
const firstScope = scope("fixture"),
  otherScope = scope("other-server", true);
window.setConnection = (value) => firstScope.setConnection(value);
window.connectionListeners = () => firstScope.connections.size;
window.repeatLive = () => {
  for (const accept of firstScope.connections) accept("live", "open", {});
};
window.phase = (id, phase) => {
  const old = payloads[id],
    ref = refs[id];
  const next = { ...old, agent: { ...old.agent!, activity: { phase } } };
  payloads[id] = next;
  firstScope.resources.publish(ref, fixtureData<typeof ref>(next));
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
  window.renders[id] = (window.renders[id] ?? 0) + 1;
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
  window.selectChat = select;
  window.selectServer = selectServer;
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
const root = document.getElementById("root")!;
createRoot(root).render(<Harness />);

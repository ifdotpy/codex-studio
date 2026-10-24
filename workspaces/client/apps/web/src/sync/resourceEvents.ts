import {
  openServerEvents,
  type ServerEventSource,
} from "../servers/eventSource";

import { serverStorageName } from "../servers/environment";

import {
  SYNC_PROTOCOL_VERSION,
  type components,
  type operations,
} from "../generated/api";

import { SYNC_STREAM_EVENT_MAX_BYTES } from "../generated/sharedValues";

import { API_SCHEMA_HASH, API_SCHEMA_HASH_PARAM } from "../generated/apiSchema";

import {
  clearSchemaUpdateAttemptAfterMatch,
  get,
  isApiSchemaMismatch,
  isApiSchemaMismatchHandshake,
  matchingApiSchemaResponseGeneration,
  markApiSchemaMismatch,
  onApiSchemaMismatch,
  post,
} from "../api";

import { persistResourceEntityChanges, syncDatabase } from "./client";

import { ResourceEntityDelivery } from "./resourceEntityDelivery";

import { onResume } from "./resume";

import { retryableReadError } from "./readRetry";

import {
  configureTokenRateStream,
  receiveResourceTokenRates,
  getWorkspaceTokenRates,
} from "../usage/tokenRate";

export type ResourceRef = components["schemas"]["ResourceRef"];

export type ResourceChangeEvent = components["schemas"]["ResourceChangeEvent"];

export type ResourceHeartbeatEvent =
  components["schemas"]["ResourceHeartbeatEvent"];

export type ResourceTokenRatesEvent =
  components["schemas"]["ResourceTokenRatesEvent"];

export type ResourceVersion = {
  epoch: string;
  revision: number;
  reason?: ResourceChangeEvent["reason"];
  draftChanges?: components["schemas"]["DraftChangeBatch"];
  /** Highest entity row represented by this StateResource revision, when present. */
  entitySequence?: number;
  /** Transcript cursor persisted before this notification; readers verify local coverage. */
  transcriptSequence?: number;
  /** The server cannot enumerate the exact changed rows; clients must reconcile. */
  entitySequenceReset?: boolean;
};

type Version = ResourceVersion;

export type ResourceConnectionState =
  | "connecting"
  | "live"
  | "degraded"
  | "offline"
  | "schema-mismatch";

export type ResourceStreamState = "stopped" | "connecting" | "open";

type Listener = (version?: ResourceVersion) => void;

type TabSubscriptions = {
  kind: "subscriptions";
  workspaceId: string;
  tabId: string;
  resources: unknown;
  tokenRates: unknown;
  reset: unknown;
};

type TabEvent = {
  kind: "resource-event";
  workspaceId: string;
  tabId: string;
  event: unknown;
};

type TabTokenRates = {
  kind: "token-rates";
  workspaceId: string;
  tabId: string;
  event: unknown;
};

type TabHeartbeat = {
  kind: "tab-heartbeat";
  workspaceId: string;
  tabId: string;
};

type LeaderHeartbeat = {
  kind: "leader-heartbeat";
  workspaceId: string;
  tabId: string;
};

type TabStatus = {
  kind: "status";
  workspaceId: string;
  tabId: string;
  status: ResourceConnectionState;
};

type TabSubscriptionDiscovery = {
  kind: "discover-subscriptions";
  workspaceId: string;
  tabId: string;
};

type TabMessage = (
  | TabSubscriptions
  | TabEvent
  | TabTokenRates
  | TabHeartbeat
  | LeaderHeartbeat
  | TabStatus
  | TabSubscriptionDiscovery
) & { apiSchemaHash: string };

type OutgoingMessage =
  | {
      kind: "subscriptions";
      resources: ResourceRef[];
      tokenRates: boolean;
      reset: boolean;
    }
  | { kind: "resource-event"; event: ResourceChangeEvent }
  | { kind: "token-rates"; event: ResourceTokenRatesEvent }
  | { kind: "tab-heartbeat" }
  | { kind: "leader-heartbeat" }
  | { kind: "discover-subscriptions" }
  | { kind: "status"; status: ResourceConnectionState };
export type ResourceEventsOptions = {
  api: Pick<
    import("../api").ServerApi,
    | "clearSchemaUpdateAttemptAfterMatch"
    | "get"
    | "post"
    | "isApiSchemaMismatch"
    | "matchingApiSchemaResponseGeneration"
    | "markApiSchemaMismatch"
    | "onApiSchemaMismatch"
  >;
  sync: Pick<
    ReturnType<typeof import("./client").createSyncClient>,
    "persistResourceEntityChanges" | "syncDatabase"
  >;
  tokenRates: Pick<
    ReturnType<typeof import("../usage/tokenRate").createTokenRateCache>,
    | "configureTokenRateStream"
    | "receiveResourceTokenRates"
    | "getWorkspaceTokenRates"
  >;
  openEvents: typeof openServerEvents;
  storageName: typeof serverStorageName;
};
export function createResourceEvents(options: ResourceEventsOptions) {
  const {
    clearSchemaUpdateAttemptAfterMatch,
    get,
    isApiSchemaMismatch,
    matchingApiSchemaResponseGeneration,
    markApiSchemaMismatch,
    onApiSchemaMismatch,
    post,
  } = options.api;
  const { persistResourceEntityChanges, syncDatabase } = options.sync;
  const {
    configureTokenRateStream,
    receiveResourceTokenRates,
    getWorkspaceTokenRates,
  } = options.tokenRates;
  const openServerEvents = options.openEvents;
  const serverStorageName = options.storageName;

  const HEARTBEAT_TIMEOUT_MS = 45_000;

  const SCHEMA_HANDSHAKE_TIMEOUT_MS = 15_000;

  const SCHEMA_HANDSHAKE_FAILURE_LIMIT = 3;

  const PEER_HEARTBEAT_MS = 3_000;

  const PEER_TIMEOUT_MS = 10_000;

  const BASE_RETRY_MS = 500;

  const MAX_RETRY_MS = 15_000;

  const COORDINATOR_IDLE_STOP_MS = 1_000;

  const RESOURCE_FLUSH_MS = 20;

  const SUBSCRIPTION_DEBOUNCE_MS = 30;

  const SUBSCRIPTION_ACK_TIMEOUT_MS = 10_000;

  const MAX_INACTIVE_RESOURCE_VERSIONS = 128;

  const subscribers = new Map<string, Set<Listener>>();

  const baselineSubscribers = new Map<string, Set<Listener>>();

  const resourceRefs = new Map<string, ResourceRef>();

  const resourceValues = new Map<string, Version>();

  const baselineReconciliations = new Set<string>();

  const deliveredEntitySequenceBatches = new Set<string>();

  const transportStatusListeners = new Set<
    (status: ResourceConnectionState, stream: ResourceStreamState) => void
  >();

  const tokenRateListeners = new Set<
    (event: ResourceTokenRatesEvent) => void
  >();

  const peerSubscriptions = new Map<
    string,
    { resources: ResourceRef[]; tokenRates: boolean; seenAt: number }
  >();

  let workspaceId: string | undefined;

  let tabId = "";

  let channel: BroadcastChannel | undefined;

  let source: ServerEventSource | undefined;

  let sourceOpened = false;
  let sessionId: string | undefined;
  let subscriptionGen = 0;
  let subscriptionGeneration = 0;
  let wireResources: ResourceRef[] = [];
  let wireTokenRates = false;
  let acceptedResources = new Set<string>();
  const blockedResources = new Map<string, ResourceRef>();
  let blockedSelectors: ResourceRef[] = [];
  let pendingSubscription:
    | { gen: number; resources: ResourceRef[]; tokenRates: boolean }
    | undefined;
  let subscriptionTimer: ReturnType<typeof setTimeout> | undefined;
  let subscriptionAckTimer: ReturnType<typeof setTimeout> | undefined;
  let subscriptionRequest: AbortController | undefined;

  let coordinatorActive = false;

  let coordinatorReady = false;

  let initializing = false;

  let coordinatorGeneration = 0;

  let owner = false;

  let independent = false;

  let ownerRequestPending = false;

  let ownerRequestController: AbortController | undefined;

  let releaseOwner: (() => void) | undefined;

  let peerHeartbeatTimer: ReturnType<typeof setInterval> | undefined;

  let heartbeatTimeout: ReturnType<typeof setTimeout> | undefined;

  let schemaHandshakeTimeout: ReturnType<typeof setTimeout> | undefined;

  let reconnectTimer: ReturnType<typeof setTimeout> | undefined;

  let flushGeneration = 0;

  let flushTimer: ReturnType<typeof setTimeout> | undefined;

  let coordinatorIdleStopTimer: ReturnType<typeof setTimeout> | undefined;

  let retryCount = 0;

  let preHandshakeFailures = 0;

  let preHandshakeResponseGeneration: number | undefined;

  let identityProbe: AbortController | undefined;

  let lastEpoch: string | undefined;

  let lastRevision: number | undefined;

  let lastHeartbeatRevision: number | undefined;

  let lastTokenEpoch: string | undefined;

  let lastTokenRevision: number | undefined;

  let lastTokenEvent: ResourceTokenRatesEvent | undefined;

  let lastLeaderHeartbeatAt = 0;

  let streamUpdateGeneration = 0;

  let pendingResources = new Set<string>();

  let pendingBaselines = new Set<string>();

  let pendingReset = false;

  let currentStatus: ResourceConnectionState = "connecting";

  let stopResume: (() => void) | undefined;

  let onlineHandler: (() => void) | undefined;

  let visibilityHandler: (() => void) | undefined;

  function resourceKey(resource: ResourceRef): string {
    const canonical = (value: unknown): unknown => {
      if (Array.isArray(value)) return value.map(canonical);
      if (value && typeof value === "object") {
        return Object.fromEntries(
          Object.entries(value)
            .sort(([left], [right]) => left.localeCompare(right))
            .map(([key, entry]) => [key, canonical(entry)]),
        );
      }
      return value;
    };
    return JSON.stringify(canonical(resource)) || "";
  }

  function setStatus(status: ResourceConnectionState) {
    if (currentStatus === status) return;
    currentStatus = status;
    if (status === "degraded" || status === "offline")
      requireBaselineReconciliation();
    for (const listener of transportStatusListeners) {
      try {
        listener(status, streamState());
      } catch {
        // A status consumer cannot interrupt transport recovery.
      }
    }
    if (owner || independent) broadcast({ kind: "status", status });
  }

  function streamState(): ResourceStreamState {
    if (!source)
      return coordinatorActive || initializing ? "connecting" : "stopped";
    return sourceOpened ? "open" : "connecting";
  }

  function notifyConnectionListeners() {
    const stream = streamState();
    for (const listener of transportStatusListeners) {
      try {
        listener(currentStatus, stream);
      } catch {
        // A status consumer cannot interrupt transport recovery.
      }
    }
  }

  function broadcast(message: OutgoingMessage) {
    if (!channel || !workspaceId) return;
    try {
      channel.postMessage({
        ...message,
        apiSchemaHash: API_SCHEMA_HASH,
        workspaceId,
        tabId,
      } satisfies TabMessage);
    } catch {
      releaseStream();
      stopChannel();
      setStatus("degraded");
      startIndependent();
    }
  }

  function stopChannel() {
    if (channel) {
      channel.onmessage = null;
      channel.close();
      channel = undefined;
    }
    if (peerHeartbeatTimer !== undefined) clearInterval(peerHeartbeatTimer);
    peerHeartbeatTimer = undefined;
    peerSubscriptions.clear();
  }

  function localResources(): ResourceRef[] {
    return [...resourceRefs.values()];
  }

  function aggregateResources(): ResourceRef[] {
    const all = new Map<string, ResourceRef>();
    for (const resource of localResources())
      all.set(resourceKey(resource), resource);
    for (const peer of peerSubscriptions.values())
      for (const resource of peer.resources)
        all.set(resourceKey(resource), resource);
    return [...all.entries()]
      .sort(([left], [right]) => left.localeCompare(right))
      .map(([, resource]) => resource);
  }

  function hasTokenRateInterest() {
    return (
      tokenRateListeners.size > 0 ||
      [...peerSubscriptions.values()].some((peer) => peer.tokenRates)
    );
  }

  function dispatchResource(
    resource: ResourceRef,
    version: Version,
    force = false,
  ) {
    const key = resourceKey(resource);
    if (force) resourceValues.set(key, version);
    else if (!rememberResourceVersion(resource, version)) return;
    // Draft data is admitted to RxDB in stream order. Hint coalescing may drop
    // intermediate versions, which would turn a complete interval into a gap.
    if (resource.kind === "drafts" && version.reason === "change") {
      for (const listener of subscribers.get(key) ?? []) listener(version);
      return;
    }
    pendingResources.add(key);
  }

  function entitySequenceBatchKey(sequences: number[]) {
    return [...sequences].sort((left, right) => left - right).join(",");
  }

  function rememberResourceVersion(resource: ResourceRef, version: Version) {
    const key = resourceKey(resource);
    const previous = resourceValues.get(key);
    if (
      previous?.epoch === version.epoch &&
      previous.revision >= version.revision
    ) {
      if (previous.revision === version.revision) {
        if (version.reason) previous.reason = version.reason;
        resourceValues.delete(key);
        resourceValues.set(key, previous);
      }
      return false;
    }
    resourceValues.set(key, version);
    return true;
  }

  function acknowledgeEntitySequences(
    sequences: number[],
    incomingWorkspaceId?: string,
  ) {
    if (incomingWorkspaceId && incomingWorkspaceId !== workspaceId) return;
    const exactBatch = [...new Set(sequences)]
      .filter((sequence) => Number.isSafeInteger(sequence) && sequence >= 0)
      .sort((left, right) => left - right);
    if (!exactBatch.length) return;
    deliveredEntitySequenceBatches.add(exactBatch.join(","));
    while (deliveredEntitySequenceBatches.size > 128) {
      const oldest = deliveredEntitySequenceBatches.values().next().value;
      if (oldest === undefined) break;
      deliveredEntitySequenceBatches.delete(oldest);
    }
  }

  function pruneResourceVersions(active: Set<string>) {
    let inactiveCount = [...resourceValues.keys()].filter(
      (key) => !active.has(key),
    ).length;
    if (inactiveCount <= MAX_INACTIVE_RESOURCE_VERSIONS) return;
    for (const key of resourceValues.keys()) {
      if (inactiveCount <= MAX_INACTIVE_RESOURCE_VERSIONS) break;
      if (active.has(key)) continue;
      resourceValues.delete(key);
      inactiveCount--;
    }
  }

  function requireBaselineReconciliation() {
    for (const resource of localResources())
      baselineReconciliations.add(resourceKey(resource));
  }

  function sameResource(left: ResourceRef, right: ResourceRef) {
    if (left.kind !== right.kind) return false;
    // ResourceRef selectors have only scalar fields, owned by OpenAPI.
    for (const field in left) {
      const key = field as keyof ResourceRef;
      if (left[key] !== right[key]) return false;
    }
    return true;
  }

  function currentResourceEvent(event: ResourceChangeEvent) {
    if (!blockedSelectors.length) return event;
    // The transport owns this parsed frame. Compact rejected selectors in place:
    // no key serialization, cloning, or extra arrays on the event hot path.
    let kept = 0;
    const entries = event.resourceVersions;
    const entryCount = entries?.length ?? 0;
    for (let index = 0; index < event.resources.length; index++) {
      const resource = event.resources[index]!;
      let blocked = false;
      for (let removed = 0; removed < blockedSelectors.length; removed++) {
        if (sameResource(resource, blockedSelectors[removed]!)) {
          blocked = true;
          break;
        }
      }
      if (blocked) continue;
      event.resources[kept] = resource;
      if (entries && index < entryCount) entries[kept] = entries[index]!;
      kept++;
    }
    event.resources.length = kept;
    if (entries) entries.length = Math.min(kept, entryCount);
    return event;
  }

  function dispatchEvent(received: ResourceChangeEvent) {
    const event = currentResourceEvent(received);
    if (!workspaceId || event.workspaceId !== workspaceId) {
      setStatus("degraded");
      return;
    }
    const epochChanged = lastEpoch !== event.epoch;
    if (epochChanged) {
      lastEpoch = event.epoch;
      lastRevision = undefined;
      lastHeartbeatRevision = undefined;
      lastTokenEpoch = undefined;
      lastTokenRevision = undefined;
      lastTokenEvent = undefined;
      resourceValues.clear();
      deliveredEntitySequenceBatches.clear();
    }
    const staleRevision =
      lastRevision !== undefined && event.revision < lastRevision;
    const isDuplicate = event.revision === lastRevision;
    if (!staleRevision && !isDuplicate) lastRevision = event.revision;
    if (source) refreshHeartbeatTimeout();
    const active = new Set(aggregateResources().map(resourceKey));
    const versions = event.resourceVersions?.length
      ? new Map(
          event.resourceVersions.flatMap((entry, index) => {
            const resource = entry.resource ?? event.resources[index];
            return resource
              ? [
                  [
                    resourceKey(resource),
                    { epoch: event.epoch, revision: entry.revision },
                  ] as const,
                ]
              : [];
          }),
        )
      : undefined;
    for (const [resourceIndex, resource] of event.resources.entries()) {
      const key = resourceKey(resource);
      const version: Version = versions?.get(key) ?? {
        epoch: event.epoch,
        revision: event.revision,
      };
      const revisionEntry =
        event.resourceVersions?.find(
          (entry) => entry.resource && resourceKey(entry.resource) === key,
        ) ?? event.resourceVersions?.[resourceIndex];
      if (
        resource.kind === "transcript" &&
        event.reason === "change" &&
        !epochChanged &&
        revisionEntry?.transcriptChanges
      )
        version.transcriptSequence = revisionEntry.transcriptChanges.through;
      if (resource.kind === "drafts") {
        version.reason = event.reason;
        version.draftChanges = revisionEntry?.draftChanges ?? undefined;
      }
      const entitySequences =
        resource.kind === "state" ? revisionEntry?.entitySequences : undefined;
      if (
        resource.kind === "state" &&
        revisionEntry &&
        !revisionEntry.entitySequenceReset
      )
        version.entitySequence = entitySequences?.length
          ? Math.max(...entitySequences)
          : revisionEntry.revision;
      if (resource.kind === "state" && revisionEntry?.entitySequenceReset)
        version.entitySequenceReset = true;
      if (
        !revisionEntry?.entitySequenceReset &&
        entitySequences?.length &&
        deliveredEntitySequenceBatches.delete(
          entitySequenceBatchKey(entitySequences),
        )
      ) {
        rememberResourceVersion(resource, version);
        continue;
      }
      if (active.has(key)) {
        dispatchResource(
          resource,
          version,
          !versions ||
            !!revisionEntry?.entitySequenceReset ||
            (resource.kind === "transcript" &&
              !revisionEntry?.transcriptChanges &&
              !isDuplicate &&
              !staleRevision),
        );
        if (event.reason !== "change") pendingBaselines.add(key);
      } else if (!rememberResourceVersion(resource, version) && !versions) {
        resourceValues.set(key, version);
      }
      if (event.reason !== "change" && baselineReconciliations.delete(key))
        pendingResources.add(key);
    }
    pruneResourceVersions(active);
    const local = localResources().map(resourceKey);
    if (
      event.reason !== "change" &&
      !staleRevision &&
      !isDuplicate &&
      (epochChanged ||
        event.reason === "reconnect" ||
        event.reason === "overflow" ||
        event.reason === "workspace" ||
        !versions) &&
      local.every((key) =>
        event.resources.some((resource) => resourceKey(resource) === key),
      )
    )
      pendingReset = true;
    setStatus("live");
    if (owner || independent) broadcast({ kind: "resource-event", event });
    scheduleFlush();
  }

  function tokenRateReplay(): ResourceTokenRatesEvent | undefined {
    if (!lastTokenEvent) return;
    return {
      ...lastTokenEvent,
      snapshot: true,
      removed: [],
      ...getWorkspaceTokenRates(),
    };
  }

  function receiveTokenRateEvent(value: unknown, fromPeer = false) {
    if (!hasRevisionFrame(value))
      throw new TypeError("Invalid token-rate frame");
    const event = value as ResourceTokenRatesEvent;
    if (event.workspaceId !== workspaceId) {
      setStatus("degraded");
      if (!fromPeer) reconnectNow();
      return;
    }
    if (fromPeer && (owner || independent)) return;
    if (!fromPeer) refreshHeartbeatTimeout();
    // A late follower can hear a delta before the owner answers its replay request.
    if (!event.snapshot && (lastTokenEpoch !== event.epoch || !lastTokenEvent))
      return;
    if (
      lastTokenEpoch === event.epoch &&
      lastTokenRevision !== undefined &&
      event.revision <= lastTokenRevision
    )
      return;
    if (!receiveResourceTokenRates(event))
      throw new TypeError("Invalid token-rate snapshot");
    lastTokenEpoch = event.epoch;
    lastTokenRevision = event.revision;
    lastTokenEvent = event;
    setStatus("live");
    for (const listener of tokenRateListeners) listener(event);
    if (owner || independent) broadcast({ kind: "token-rates", event });
  }

  function scheduleFlush() {
    if (flushTimer !== undefined) return;
    const generation = ++flushGeneration;
    flushTimer = setTimeout(function flushResourceChanges() {
      if (generation !== flushGeneration) return;
      flushTimer = undefined;
      try {
        if (pendingReset) {
          pendingReset = false;
          const subscribed = new Set(aggregateResources().map(resourceKey));
          for (const [key, listeners] of subscribers) {
            if (subscribed.has(key)) {
              const version = resourceValues.get(key);
              for (const listener of listeners) listener(version);
            }
          }
          pendingResources = new Set();
          pendingBaselines = new Set();
        } else {
          const delivered = new Set<string>();
          for (const key of pendingResources) {
            const version = resourceValues.get(key);
            if (!version) continue;
            for (const listener of subscribers.get(key) || [])
              listener(version);
            delivered.add(key);
          }
          for (const key of pendingBaselines) {
            if (delivered.has(key)) continue;
            const version = resourceValues.get(key);
            if (!version) continue;
            for (const listener of baselineSubscribers.get(key) || [])
              listener(version);
          }
          pendingResources = new Set();
          pendingBaselines = new Set();
        }
        retryCount = 0;
      } catch (error) {
        frameFailed(error);
      }
    }, RESOURCE_FLUSH_MS);
  }

  const entityDelivery = new ResourceEntityDelivery((event, isCurrent) => {
    const generation = subscriptionGeneration;
    return persistResourceEntityChanges(
      event,
      () =>
        isCurrent() &&
        event.workspaceId === workspaceId &&
        generation === subscriptionGeneration,
    );
  }, dispatchEvent);

  function receiveResourceEvent(value: unknown, fromPeer = false) {
    if (!hasRevisionFrame(value) || !Array.isArray(value.resources))
      throw new TypeError("Invalid resource frame");
    const event = value as ResourceChangeEvent;
    if (fromPeer && (owner || independent)) return;
    if (!workspaceId || event.workspaceId !== workspaceId) {
      setStatus("degraded");
      if (!fromPeer) reconnectNow();
      return;
    }
    entityDelivery.receive(currentResourceEvent(event));
  }

  function acceptHeartbeat(value: unknown, fromPeer = false) {
    if (!hasRevisionFrame(value))
      throw new TypeError("Invalid heartbeat frame");
    const heartbeat = value as ResourceHeartbeatEvent;
    if (heartbeat.workspaceId !== workspaceId) {
      setStatus("degraded");
      if (!fromPeer) reconnectNow();
      return;
    }
    if (lastEpoch && heartbeat.epoch !== lastEpoch) {
      lastEpoch = undefined;
      lastRevision = undefined;
      lastHeartbeatRevision = undefined;
      reconnectNow();
      return;
    }
    if (
      (lastRevision !== undefined && heartbeat.revision < lastRevision) ||
      (lastHeartbeatRevision !== undefined &&
        heartbeat.revision < lastHeartbeatRevision)
    ) {
      reconnectNow();
      return;
    }
    if (fromPeer) return;
    lastHeartbeatRevision = heartbeat.revision;
    refreshHeartbeatTimeout();
    setStatus("live");
    if (owner) broadcast({ kind: "leader-heartbeat" });
  }

  function hasRevisionFrame(value: unknown): value is Record<string, unknown> {
    return (
      !!value &&
      typeof value === "object" &&
      !Array.isArray(value) &&
      typeof (value as Record<string, unknown>).revision === "number" &&
      Number.isFinite((value as Record<string, unknown>).revision)
    );
  }

  function frameFailed(error?: unknown) {
    if (error !== undefined)
      console.error("Sync stream frame could not be applied", error);
    setStatus("degraded");
    reconnectNow();
  }

  function refreshHeartbeatTimeout() {
    clearTimeout(heartbeatTimeout);
    heartbeatTimeout = setTimeout(() => reconnectNow(), HEARTBEAT_TIMEOUT_MS);
  }

  function receiveChannelMessage(value: unknown) {
    if (!value || typeof value !== "object" || Array.isArray(value)) return;
    const record = value as Record<string, unknown>;
    if (
      record.workspaceId !== workspaceId ||
      record.apiSchemaHash !== API_SCHEMA_HASH ||
      typeof record.tabId !== "string" ||
      !record.tabId ||
      record.tabId === tabId ||
      typeof record.kind !== "string"
    )
      return;
    if (record.kind === "subscriptions") {
      if (!Array.isArray(record.resources)) return;
      const previous = peerSubscriptions.get(record.tabId);
      const priorResources = new Set(
        (previous?.resources || []).map(resourceKey),
      );
      const reset = record.reset === true;
      const added = record.resources.filter(
        (resource) => reset || !priorResources.has(resourceKey(resource)),
      );
      peerSubscriptions.set(record.tabId, {
        resources: record.resources,
        tokenRates: record.tokenRates === true,
        seenAt: Date.now(),
      });
      if (owner) {
        scheduleOwnerStreamUpdate();
        replayResourceBaseline(added);
        if (
          record.tokenRates === true &&
          (!previous?.tokenRates || reset) &&
          lastTokenEvent
        )
          broadcast({ kind: "token-rates", event: tokenRateReplay()! });
        scheduleCoordinatorIdleStop();
      }
    } else if (record.kind === "tab-heartbeat") {
      const peer = peerSubscriptions.get(record.tabId);
      if (peer) peer.seenAt = Date.now();
    } else if (record.kind === "leader-heartbeat") {
      lastLeaderHeartbeatAt = Date.now();
      const peer = peerSubscriptions.get(record.tabId);
      if (peer) peer.seenAt = Date.now();
    } else if (
      record.kind === "discover-subscriptions" &&
      !owner &&
      !independent &&
      !document.hidden &&
      navigator.onLine !== false
    ) {
      // A new lock owner has no reliable way to infer subscriptions from
      // heartbeats. Advertise them once when ownership changes so takeover
      // restores the full resource union without making quiet heartbeats reads.
      announceSubscriptions(true);
    } else if (record.kind === "resource-event") {
      receiveResourceEvent(record.event, true);
    } else if (record.kind === "token-rates") {
      receiveTokenRateEvent(record.event, true);
    } else if (
      record.kind === "status" &&
      (record.status === "connecting" ||
        record.status === "live" ||
        record.status === "degraded" ||
        record.status === "offline" ||
        record.status === "schema-mismatch")
    ) {
      if (!owner && !independent) {
        setStatus(record.status);
        if (record.status === "degraded" || record.status === "offline") {
          // A failed immediate lock attempt must not postpone failover until
          // the normal leader timeout after that leader has already left.
          lastLeaderHeartbeatAt = 0;
          startAsOwner();
        }
      }
    }
  }

  function replayResourceBaseline(resources: ResourceRef[]) {
    if (!lastEpoch || lastRevision === undefined) return;
    const groups = new Map<
      string,
      { version: Version; resources: ResourceRef[] }
    >();
    for (const resource of resources) {
      const version = resourceValues.get(resourceKey(resource));
      if (!version) continue;
      const key = `${version.epoch}:${version.revision}`;
      const group = groups.get(key) || { version, resources: [] };
      group.resources.push(resource);
      groups.set(key, group);
    }
    for (const { version, resources: known } of groups.values()) {
      const resourceVersions = known.flatMap((resource) => {
        const resourceVersion = resourceValues.get(resourceKey(resource));
        if (!resourceVersion) return [];
        return [
          {
            revision: resourceVersion.revision,
            ...(resource.kind === "state" && resourceVersion.entitySequenceReset
              ? { entitySequenceReset: true }
              : resource.kind === "state" &&
                  resourceVersion.entitySequence !== undefined
                ? { entitySequences: [resourceVersion.entitySequence] }
                : {}),
          },
        ];
      });
      broadcast({
        kind: "resource-event",
        event: {
          protocol: SYNC_PROTOCOL_VERSION,
          workspaceId: workspaceId!,
          epoch: version.epoch,
          revision: version.revision,
          reason: "initial",
          resources: known,
          resourceVersions,
        },
      });
    }
  }

  function announceSubscriptions(reset = false) {
    if (!coordinatorReady || !workspaceId) return;
    broadcast({
      kind: "subscriptions",
      resources: localResources(),
      tokenRates: tokenRateListeners.size > 0,
      reset,
    });
    if (owner || independent) scheduleOwnerStreamUpdate();
  }

  function scheduleOwnerStreamUpdate() {
    const active = new Set(aggregateResources().map(resourceKey));
    let removed = false;
    for (const resource of pendingSubscription?.resources ?? wireResources) {
      const key = resourceKey(resource);
      if (!active.has(key) && !blockedResources.has(key)) {
        acceptedResources.delete(key);
        blockedResources.set(key, resource);
        resourceValues.delete(key);
        subscriptionGeneration++;
        removed = true;
      }
    }
    if (removed) {
      blockedSelectors = [...blockedResources.values()];
      entityDelivery.invalidate();
    }
    const generation = ++streamUpdateGeneration;
    queueMicrotask(() => {
      if (generation !== streamUpdateGeneration) return;
      if (!source) updateOwnerStream();
      else if (subscriptionTimer === undefined)
        subscriptionTimer = setTimeout(() => {
          subscriptionTimer = undefined;
          updateOwnerStream();
        }, SUBSCRIPTION_DEBOUNCE_MS);
    });
  }

  function scheduleCoordinatorIdleStop() {
    if (coordinatorIdleStopTimer !== undefined)
      clearTimeout(coordinatorIdleStopTimer);
    coordinatorIdleStopTimer = setTimeout(() => {
      coordinatorIdleStopTimer = undefined;
      const now = Date.now();
      for (const [peerId, peer] of peerSubscriptions) {
        if (now - peer.seenAt > PEER_TIMEOUT_MS)
          peerSubscriptions.delete(peerId);
      }
      if (
        !subscribers.size &&
        !tokenRateListeners.size &&
        (!owner || (!aggregateResources().length && !hasTokenRateInterest()))
      )
        stopCoordinator();
    }, COORDINATOR_IDLE_STOP_MS);
  }

  function closeSource() {
    clearTimeout(subscriptionTimer);
    subscriptionTimer = undefined;
    clearTimeout(subscriptionAckTimer);
    subscriptionAckTimer = undefined;
    subscriptionRequest?.abort();
    subscriptionRequest = undefined;
    sessionId = undefined;
    pendingSubscription = undefined;
    subscriptionGen = 0;
    subscriptionGeneration++;
    acceptedResources.clear();
    entityDelivery.invalidate();
    blockedResources.clear();
    blockedSelectors = [];
    clearTimeout(heartbeatTimeout);
    heartbeatTimeout = undefined;
    clearTimeout(schemaHandshakeTimeout);
    schemaHandshakeTimeout = undefined;
    source?.close();
    source = undefined;
    sourceOpened = false;
  }

  function cancelIdentityProbe() {
    const probe = identityProbe;
    identityProbe = undefined;
    probe?.abort();
  }

  function retryDelay() {
    const ceiling = Math.min(BASE_RETRY_MS * 2 ** retryCount, MAX_RETRY_MS);
    retryCount++;
    return Math.max(100, Math.round(ceiling * (0.5 + Math.random())));
  }

  function scheduleReconnect() {
    if (
      reconnectTimer !== undefined ||
      !coordinatorActive ||
      document.hidden ||
      navigator.onLine === false
    )
      return;
    setStatus("degraded");
    reconnectTimer = setTimeout(() => {
      reconnectTimer = undefined;
      if (!coordinatorReady) void initialize();
      else if (owner || independent) openSource();
    }, retryDelay());
  }

  function reconnectNow() {
    if (source) requireBaselineReconciliation();
    closeSource();
    scheduleReconnect();
  }

  function openSource() {
    if (
      isApiSchemaMismatch() ||
      !coordinatorActive ||
      (!owner && !independent) ||
      document.hidden ||
      navigator.onLine === false
    )
      return;
    const resources = owner ? aggregateResources() : localResources();
    const tokenRates = owner
      ? hasTokenRateInterest()
      : tokenRateListeners.size > 0;
    if (!resources.length && !tokenRates) {
      cancelIdentityProbe();
      closeSource();
      return;
    }
    if (source) {
      updateSubscriptions(resources, tokenRates);
      return;
    }
    closeSource();
    wireResources = resources;
    wireTokenRates = tokenRates;
    acceptedResources = new Set(resources.map(resourceKey));
    setStatus("connecting");
    try {
      const subscription: NonNullable<
        operations["sync_stream_api_sync_stream_get"]["parameters"]["query"]
      > = {
        protocol: String(SYNC_PROTOCOL_VERSION),
        resources: JSON.stringify(resources),
        tokenRates,
        [API_SCHEMA_HASH_PARAM]: API_SCHEMA_HASH,
      };
      const query = new URLSearchParams(
        Object.entries(subscription).map(([key, value]) => [
          key,
          String(value),
        ]),
      );
      const connected = openServerEvents(`/api/sync/stream?${query}`);
      source = connected;
      notifyConnectionListeners();
      let schemaHandshakeReceived = false;
      let connectionOpened = false;
      const failHandshake = () => {
        if (source !== connected || schemaHandshakeReceived) return;
        requireBaselineReconciliation();
        closeSource();
        if (preHandshakeFailures === 0)
          preHandshakeResponseGeneration =
            matchingApiSchemaResponseGeneration();
        preHandshakeFailures++;
        if (preHandshakeFailures >= SCHEMA_HANDSHAKE_FAILURE_LIMIT) {
          if (
            preHandshakeResponseGeneration !== undefined &&
            matchingApiSchemaResponseGeneration() >
              preHandshakeResponseGeneration
          ) {
            scheduleReconnect();
          } else markApiSchemaMismatch();
        } else scheduleReconnect();
      };
      connected.onopen = () => {
        if (source !== connected) return;
        sourceOpened = true;
        notifyConnectionListeners();
        connectionOpened = true;
        clearTimeout(schemaHandshakeTimeout);
        if (!schemaHandshakeReceived)
          schemaHandshakeTimeout = setTimeout(
            failHandshake,
            SCHEMA_HANDSHAKE_TIMEOUT_MS,
          );
      };
      const applyFrame = (
        message: Event,
        apply: (parsed: unknown) => void,
        resetRetryOnSuccess = true,
        maxBytes?: number,
      ) => {
        if (source !== connected) return;
        if (!schemaHandshakeReceived) {
          markApiSchemaMismatch();
          return;
        }
        try {
          const data = (message as MessageEvent<string>).data;
          // Check received UTF-8 once, before parsing or publishing inline batches.
          if (
            maxBytes !== undefined &&
            new TextEncoder().encode(data).length > maxBytes
          )
            throw new TypeError("Resource frame exceeds the size limit");
          apply(JSON.parse(data) as unknown);
          if (resetRetryOnSuccess) retryCount = 0;
        } catch (error) {
          frameFailed(error);
        }
      };
      connected.addEventListener("session", (message: Event) => {
        if (source !== connected) return;
        try {
          const event = JSON.parse(
            (message as MessageEvent<string>).data,
          ) as components["schemas"]["SyncSessionEvent"];
          if (
            typeof event.sessionId !== "string" ||
            !event.sessionId ||
            sessionId
          )
            throw new TypeError("Invalid sync session");
          sessionId = event.sessionId;
          scheduleOwnerStreamUpdate();
        } catch (error) {
          frameFailed(error);
        }
      });
      connected.addEventListener("subscriptions", (message: Event) => {
        applyFrame(message, (value) => {
          const ack = value as components["schemas"]["SyncSubscriptionAck"];
          const pending = pendingSubscription;
          if (!pending || ack.gen !== pending.gen)
            throw new TypeError("Unexpected subscription acknowledgement");
          clearTimeout(subscriptionAckTimer);
          subscriptionAckTimer = undefined;
          pendingSubscription = undefined;
          wireResources = pending.resources;
          wireTokenRates = pending.tokenRates;
          const active = new Set(aggregateResources().map(resourceKey));
          acceptedResources = new Set(
            wireResources.map(resourceKey).filter((key) => active.has(key)),
          );
          const acknowledged = new Set(wireResources.map(resourceKey));
          for (const key of blockedResources.keys()) {
            // SSE order makes pre-removal data impossible after this ack. A
            // re-added selector stays blocked until its own addition is acked.
            if (
              acceptedResources.has(key) ||
              (!active.has(key) && !acknowledged.has(key))
            )
              blockedResources.delete(key);
          }
          blockedSelectors = [...blockedResources.values()];
          refreshHeartbeatTimeout();
          scheduleOwnerStreamUpdate();
        });
      });
      connected.addEventListener("api-schema", (message: Event) => {
        if (source !== connected) return;
        try {
          const payload: unknown = JSON.parse(
            (message as MessageEvent<string>).data,
          );
          if (isApiSchemaMismatchHandshake(payload)) {
            markApiSchemaMismatch((payload as { hash?: unknown } | null)?.hash);
            return;
          }
          schemaHandshakeReceived = true;
          cancelIdentityProbe();
          preHandshakeFailures = 0;
          preHandshakeResponseGeneration = undefined;
          clearSchemaUpdateAttemptAfterMatch();
          clearTimeout(schemaHandshakeTimeout);
          schemaHandshakeTimeout = undefined;
        } catch {
          markApiSchemaMismatch();
        }
      });
      connected.addEventListener("resources", (message: Event) => {
        applyFrame(
          message,
          (parsed) => {
            if (!hasRevisionFrame(parsed) || !Array.isArray(parsed.resources))
              throw new TypeError("Invalid resource frame");
            receiveResourceEvent(parsed);
          },
          false,
          SYNC_STREAM_EVENT_MAX_BYTES,
        );
      });
      connected.addEventListener("heartbeat", (message: Event) => {
        applyFrame(message, acceptHeartbeat);
      });
      connected.addEventListener("token-rates", (message: Event) => {
        applyFrame(message, receiveTokenRateEvent);
      });
      connected.onerror = () => {
        if (source !== connected) return;
        if (connectionOpened && !schemaHandshakeReceived) {
          failHandshake();
          return;
        }
        requireBaselineReconciliation();
        closeSource();
        if (!connectionOpened && !identityProbe) {
          // EventSource hides HTTP rejection details. Check the identity once
          // to distinguish a server rollback from a temporary network failure.
          const probe = new AbortController();
          identityProbe = probe;
          void get("/api/sync/identity", {
            timeoutMs: 5_000,
            cache: "no-store",
            signal: probe.signal,
          })
            .catch(() => {})
            .finally(() => {
              if (identityProbe === probe) identityProbe = undefined;
            });
        }
        scheduleReconnect();
      };
      if (!heartbeatTimeout) {
        heartbeatTimeout = setTimeout(
          () => reconnectNow(),
          HEARTBEAT_TIMEOUT_MS,
        );
      }
    } catch {
      closeSource();
      scheduleReconnect();
    }
  }

  function updateSubscriptions(resources: ResourceRef[], tokenRates: boolean) {
    if (!source || pendingSubscription || subscriptionRequest) return;
    const desired = new Set(resources.map(resourceKey));
    const previous = new Set(wireResources.map(resourceKey));
    const add = resources.filter(
      (resource) =>
        !previous.has(resourceKey(resource)) ||
        !acceptedResources.has(resourceKey(resource)),
    );
    const remove = wireResources.filter(
      (resource) =>
        !desired.has(resourceKey(resource)) ||
        !acceptedResources.has(resourceKey(resource)),
    );
    if (!add.length && !remove.length && tokenRates === wireTokenRates) return;
    if (!sessionId) {
      // A stream without its session cannot acknowledge a diff. Recover with
      // the complete desired set rather than stranding newly added resources.
      reconnectNow();
      return;
    }
    const connected = source;
    const gen = ++subscriptionGen;
    pendingSubscription = { gen, resources, tokenRates };
    const controller = new AbortController();
    subscriptionRequest = controller;
    subscriptionAckTimer = setTimeout(() => {
      if (source === connected) reconnectNow();
    }, SUBSCRIPTION_ACK_TIMEOUT_MS);
    void post(
      "/api/sync/subscriptions",
      { sessionId, gen, add, remove, tokenRates },
      { signal: controller.signal, timeoutMs: SUBSCRIPTION_ACK_TIMEOUT_MS },
    )
      .catch(() => {
        // A lost response has an unknown outcome. Reopen instead of retrying a diff.
        if (source === connected) reconnectNow();
      })
      .finally(() => {
        if (subscriptionRequest !== controller) return;
        subscriptionRequest = undefined;
        if (source === connected) scheduleOwnerStreamUpdate();
      });
  }

  function updateOwnerStream() {
    openSource();
  }

  function sendPeerHeartbeat() {
    if (!coordinatorReady || document.hidden || navigator.onLine === false)
      return;
    broadcast({ kind: "tab-heartbeat" });
    if (owner) {
      broadcast({ kind: "leader-heartbeat" });
      const now = Date.now();
      let removedPeer = false;
      for (const [peerId, peer] of peerSubscriptions) {
        if (now - peer.seenAt > PEER_TIMEOUT_MS) {
          peerSubscriptions.delete(peerId);
          removedPeer = true;
        }
      }
      scheduleOwnerStreamUpdate();
      if (removedPeer) scheduleCoordinatorIdleStop();
    } else if (
      channel &&
      navigator.locks &&
      Date.now() - lastLeaderHeartbeatAt > PEER_TIMEOUT_MS
    ) {
      setStatus("degraded");
      lastLeaderHeartbeatAt = Date.now();
      startAsOwner();
    }
  }

  function startPeerHeartbeat() {
    if (peerHeartbeatTimer !== undefined) clearInterval(peerHeartbeatTimer);
    peerHeartbeatTimer = setInterval(sendPeerHeartbeat, PEER_HEARTBEAT_MS);
  }

  function startIndependent() {
    if (!coordinatorActive || owner || independent) return;
    independent = true;
    startPeerHeartbeat();
    scheduleOwnerStreamUpdate();
  }

  function startAsOwner() {
    if (!coordinatorActive || owner || independent || ownerRequestPending)
      return;
    lastLeaderHeartbeatAt = Date.now();
    const locks = navigator.locks;
    if (!channel || !locks || !workspaceId) {
      startIndependent();
      return;
    }
    const requestController = new AbortController();
    const generation = coordinatorGeneration;
    const requestedWorkspaceId = workspaceId;
    ownerRequestController = requestController;
    ownerRequestPending = true;
    void locks
      .request(
        serverStorageName(
          `codex-sync-stream:${location.origin}:${requestedWorkspaceId}`,
        ),
        { mode: "exclusive", signal: requestController.signal },
        async (lock) => {
          if (ownerRequestController === requestController) {
            ownerRequestController = undefined;
            ownerRequestPending = false;
          }
          if (
            !coordinatorActive ||
            generation !== coordinatorGeneration ||
            requestedWorkspaceId !== workspaceId ||
            !lock ||
            document.hidden ||
            navigator.onLine === false
          )
            return;
          requireBaselineReconciliation();
          owner = true;
          startPeerHeartbeat();
          broadcast({ kind: "discover-subscriptions" });
          broadcast({ kind: "status", status: "connecting" });
          scheduleOwnerStreamUpdate();
          await new Promise<void>((resolve) => {
            releaseOwner = resolve;
          });
          releaseOwner = undefined;
          owner = false;
          closeSource();
          if (peerHeartbeatTimer !== undefined)
            clearInterval(peerHeartbeatTimer);
          peerHeartbeatTimer = undefined;
        },
      )
      .catch((error: unknown) => {
        if (ownerRequestController === requestController) {
          ownerRequestController = undefined;
          ownerRequestPending = false;
        }
        if (
          requestController.signal.aborted ||
          (error instanceof DOMException && error.name === "AbortError") ||
          !coordinatorActive ||
          generation !== coordinatorGeneration ||
          requestedWorkspaceId !== workspaceId
        )
          return;
        startIndependent();
      });
  }

  function releaseStream() {
    cancelIdentityProbe();
    const pendingRequest = ownerRequestController;
    ownerRequestController = undefined;
    ownerRequestPending = false;
    pendingRequest?.abort();
    const release = releaseOwner;
    releaseOwner = undefined;
    if (source) requireBaselineReconciliation();
    owner = false;
    independent = false;
    closeSource();
    if (peerHeartbeatTimer !== undefined) clearInterval(peerHeartbeatTimer);
    peerHeartbeatTimer = undefined;
    if (reconnectTimer !== undefined) clearTimeout(reconnectTimer);
    reconnectTimer = undefined;
    release?.();
  }

  async function initialize() {
    if (coordinatorReady || initializing || !coordinatorActive) return;
    initializing = true;
    const generation = coordinatorGeneration;
    try {
      const storage = await syncDatabase();
      if (!coordinatorActive || generation !== coordinatorGeneration) return;
      workspaceId = storage.workspaceId;
      tabId ||=
        globalThis.crypto?.randomUUID?.() || `${Date.now()}-${Math.random()}`;
      coordinatorReady = true;
      try {
        if (typeof BroadcastChannel !== "undefined") {
          channel = new BroadcastChannel(
            serverStorageName(`codex-sync-${location.origin}-${workspaceId}`),
          );
          channel.onmessage = (event: MessageEvent<unknown>) => {
            try {
              receiveChannelMessage(event.data);
            } catch (error) {
              frameFailed(error);
            }
          };
        }
      } catch {
        channel = undefined;
      }
      startPeerHeartbeat();
      announceSubscriptions();
      if (!channel || !navigator.locks) startIndependent();
      else startAsOwner();
    } catch (error) {
      if (generation === coordinatorGeneration) {
        setStatus(navigator.onLine === false ? "offline" : "degraded");
        if (retryableReadError(error)) scheduleReconnect();
      }
    } finally {
      if (generation === coordinatorGeneration) initializing = false;
    }
  }

  function resumeTransport() {
    if (!coordinatorActive) return;
    if (document.hidden || navigator.onLine === false) {
      setStatus(navigator.onLine === false ? "offline" : "degraded");
      releaseStream();
      return;
    }
    if (!coordinatorReady) void initialize();
    else {
      // A resumed follower must keep advertising its subscriptions even when
      // another tab still holds the stream lock.
      startPeerHeartbeat();
      if (!owner && !independent) startAsOwner();
    }
    if (owner || independent) scheduleOwnerStreamUpdate();
    announceSubscriptions(true);
  }

  function stopCoordinator() {
    coordinatorActive = false;
    coordinatorGeneration++;
    initializing = false;
    if (coordinatorIdleStopTimer !== undefined)
      clearTimeout(coordinatorIdleStopTimer);
    coordinatorIdleStopTimer = undefined;
    streamUpdateGeneration++;
    releaseStream();
    flushGeneration += 1;
    if (flushTimer !== undefined) clearTimeout(flushTimer);
    flushTimer = undefined;
    clearTimeout(heartbeatTimeout);
    clearTimeout(reconnectTimer);
    stopChannel();
    stopResume?.();
    stopResume = undefined;
    if (onlineHandler) window.removeEventListener("offline", onlineHandler);
    if (visibilityHandler)
      document.removeEventListener("visibilitychange", visibilityHandler);
    onlineHandler = undefined;
    visibilityHandler = undefined;
    coordinatorReady = false;
    workspaceId = undefined;
    lastEpoch = undefined;
    lastRevision = undefined;
    lastHeartbeatRevision = undefined;
    lastTokenEpoch = undefined;
    lastTokenRevision = undefined;
    lastTokenEvent = undefined;
    resourceValues.clear();
    baselineReconciliations.clear();
    deliveredEntitySequenceBatches.clear();
    resourceRefs.clear();
    peerSubscriptions.clear();
    setStatus(isApiSchemaMismatch() ? "schema-mismatch" : "connecting");
    notifyConnectionListeners();
  }

  function startCoordinator() {
    if (coordinatorActive || isApiSchemaMismatch()) return;
    coordinatorActive = true;
    coordinatorGeneration++;
    stopResume = onResume(resumeTransport);
    onlineHandler = () => {
      if (navigator.onLine === false) {
        setStatus("offline");
        releaseStream();
      } else resumeTransport();
    };
    visibilityHandler = () => {
      if (document.hidden) {
        setStatus("degraded");
        releaseStream();
        broadcast({
          kind: "subscriptions",
          resources: [],
          tokenRates: false,
          reset: false,
        });
      } else resumeTransport();
    };
    window.addEventListener("offline", onlineHandler);
    window.addEventListener("online", onlineHandler);
    document.addEventListener("visibilitychange", visibilityHandler);
    if (document.hidden || navigator.onLine === false) {
      setStatus(navigator.onLine === false ? "offline" : "connecting");
      return;
    }
    void initialize();
  }

  function watchResourceChanges(
    resource: ResourceRef,
    callback: (version?: ResourceVersion) => void,
    options: { waitForBaseline?: boolean } = {},
  ): () => void {
    const key = resourceKey(resource);
    if (coordinatorIdleStopTimer !== undefined)
      clearTimeout(coordinatorIdleStopTimer);
    coordinatorIdleStopTimer = undefined;
    let listeners = subscribers.get(key);
    if (!listeners) subscribers.set(key, (listeners = new Set()));
    resourceRefs.set(key, resource);
    listeners.add(callback);
    if (options.waitForBaseline) {
      let baselines = baselineSubscribers.get(key);
      if (!baselines) baselineSubscribers.set(key, (baselines = new Set()));
      baselines.add(callback);
    }
    const known = resourceValues.get(key);
    // A known version is the most recent baseline/change observed on this
    // shared stream and can be replayed immediately to a late resource reader.
    if (known) callback(known);
    startCoordinator();
    // A first local listener may attach after another tab already added this
    // resource to the shared stream union. Ask the owner for its baseline even
    // when the resource is not new to this tab's aggregate subscription list.
    announceSubscriptions(!known);
    let stopped = false;
    return () => {
      if (stopped) return;
      stopped = true;
      listeners?.delete(callback);
      const baselines = baselineSubscribers.get(key);
      baselines?.delete(callback);
      if (!baselines?.size) baselineSubscribers.delete(key);
      if (!listeners?.size) {
        subscribers.delete(key);
        resourceRefs.delete(key);
        baselineReconciliations.delete(key);
        if (!owner && !independent) {
          subscriptionGeneration++;
          entityDelivery.invalidate();
        }
      }
      announceSubscriptions();
      if (!subscribers.size && !tokenRateListeners.size) {
        cancelIdentityProbe();
        scheduleCoordinatorIdleStop();
      }
    };
  }

  function watchResourceConnection(
    listener: (
      status: ResourceConnectionState,
      stream: ResourceStreamState,
    ) => void,
  ): () => void {
    transportStatusListeners.add(listener);
    listener(currentStatus, streamState());
    return () => transportStatusListeners.delete(listener);
  }

  function watchTokenRateEvents(
    listener: (event: ResourceTokenRatesEvent) => void,
  ): () => void {
    if (coordinatorIdleStopTimer !== undefined)
      clearTimeout(coordinatorIdleStopTimer);
    coordinatorIdleStopTimer = undefined;
    tokenRateListeners.add(listener);
    if (lastTokenEvent && lastTokenEvent.workspaceId === workspaceId)
      listener(tokenRateReplay()!);
    startCoordinator();
    announceSubscriptions();
    let stopped = false;
    return () => {
      if (stopped) return;
      stopped = true;
      tokenRateListeners.delete(listener);
      announceSubscriptions();
      if (!subscribers.size && !tokenRateListeners.size)
        scheduleCoordinatorIdleStop();
    };
  }

  configureTokenRateStream(watchTokenRateEvents);

  const stopMismatch = onApiSchemaMismatch(stopCoordinator);
  return {
    acknowledgeEntitySequences,
    watchResourceChanges,
    watchResourceConnection,
    watchTokenRateEvents,
    dispose() {
      stopMismatch();
      stopCoordinator();
    },
  };
}
const defaultResourceEvents = createResourceEvents({
  api: {
    clearSchemaUpdateAttemptAfterMatch,
    get,
    isApiSchemaMismatch,
    matchingApiSchemaResponseGeneration,
    markApiSchemaMismatch,
    onApiSchemaMismatch,
    post,
  },
  sync: {
    persistResourceEntityChanges: (
      ...args: Parameters<typeof persistResourceEntityChanges>
    ) => persistResourceEntityChanges(...args),
    syncDatabase: (...args: Parameters<typeof syncDatabase>) =>
      syncDatabase(...args),
  },
  tokenRates: {
    configureTokenRateStream,
    receiveResourceTokenRates,
    getWorkspaceTokenRates,
  },
  openEvents: openServerEvents,
  storageName: serverStorageName,
});
export const {
  acknowledgeEntitySequences,
  watchResourceChanges,
  watchResourceConnection,
  watchTokenRateEvents,
} = defaultResourceEvents;

import type { components } from "../generated/api";
import {
  API_SCHEMA_HASH,
  API_SCHEMA_HASH_PARAM,
  API_SCHEMA_MISMATCH_FIELD,
} from "../generated/apiSchema";
import {
  clearSchemaUpdateAttemptAfterMatch,
  isApiSchemaMismatch,
  matchingApiSchemaResponseGeneration,
  markApiSchemaMismatch,
  onApiSchemaMismatch,
} from "../api";
import { syncDatabase } from "./client";
import { onResume } from "./resume";
import { retryableReadError } from "./readRetry";
import {
  configureTokenRateStream,
  receiveResourceTokenRates,
} from "../usage/tokenRate";

export type ResourceRef = components["schemas"]["ResourceRef"];
export type ResourceChangeEvent = components["schemas"]["ResourceChangeEvent"];
export type ResourceHeartbeatEvent =
  components["schemas"]["ResourceHeartbeatEvent"];
export type ResourceTokenRatesEvent =
  components["schemas"]["ResourceTokenRatesEvent"];

export type ResourceVersion = { epoch: string; revision: number };
type Version = ResourceVersion;
export type ResourceConnectionState =
  | "connecting"
  | "live"
  | "degraded"
  | "offline"
  | "schema-mismatch";
type Listener = (version?: ResourceVersion) => void;
type ResourceObserver = (event: ResourceChangeEvent) => void;
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

const HEARTBEAT_TIMEOUT_MS = 45_000;
const SCHEMA_HANDSHAKE_TIMEOUT_MS = 15_000;
const SCHEMA_HANDSHAKE_FAILURE_LIMIT = 3;
const PEER_HEARTBEAT_MS = 3_000;
const PEER_TIMEOUT_MS = 10_000;
const BASE_RETRY_MS = 500;
const MAX_RETRY_MS = 15_000;
const RESOURCE_FLUSH_MS = 20;
const MAX_INACTIVE_RESOURCE_VERSIONS = 128;

const subscribers = new Map<string, Set<Listener>>();
const resourceRefs = new Map<string, ResourceRef>();
const resourceValues = new Map<string, Version>();
const resourceObservers = new Set<ResourceObserver>();
const baselineReconciliations = new Set<string>();
const transportStatusListeners = new Set<
  (status: ResourceConnectionState) => void
>();
const tokenRateListeners = new Set<(event: ResourceTokenRatesEvent) => void>();
const peerSubscriptions = new Map<
  string,
  { resources: ResourceRef[]; tokenRates: boolean; seenAt: number }
>();

let workspaceId: string | undefined;
let tabId = "";
let channel: BroadcastChannel | undefined;
let source: EventSource | undefined;
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
let flushTimer: ReturnType<typeof setTimeout> | undefined;
let retryCount = 0;
let preHandshakeFailures = 0;
let preHandshakeResponseGeneration: number | undefined;
let lastEpoch: string | undefined;
let lastRevision: number | undefined;
let lastHeartbeatRevision: number | undefined;
let lastTokenEpoch: string | undefined;
let lastTokenRevision: number | undefined;
let lastTokenEvent: ResourceTokenRatesEvent | undefined;
let lastLeaderHeartbeatAt = 0;
let activeQueryKey = "";
let keepLiveOnReconfigure = false;
let pendingResources = new Set<string>();
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
      listener(status);
    } catch {
      // A status consumer cannot interrupt transport recovery.
    }
  }
  if (owner || independent) broadcast({ kind: "status", status });
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

function dispatchResource(resource: ResourceRef, version: Version) {
  if (!rememberResourceVersion(resource, version)) return;
  pendingResources.add(resourceKey(resource));
}

function rememberResourceVersion(resource: ResourceRef, version: Version) {
  const key = resourceKey(resource);
  const previous = resourceValues.get(key);
  if (
    previous?.epoch === version.epoch &&
    previous.revision >= version.revision
  ) {
    if (previous.revision === version.revision) {
      resourceValues.delete(key);
      resourceValues.set(key, previous);
    }
    return false;
  }
  resourceValues.set(key, version);
  return true;
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

function dispatchEvent(event: ResourceChangeEvent) {
  if (!workspaceId || event.workspaceId !== workspaceId) {
    setStatus("degraded");
    return;
  }
  if (lastEpoch !== event.epoch) {
    lastEpoch = event.epoch;
    lastRevision = undefined;
    lastHeartbeatRevision = undefined;
    lastTokenEpoch = undefined;
    lastTokenRevision = undefined;
    lastTokenEvent = undefined;
    resourceValues.clear();
  }
  const staleRevision =
    lastRevision !== undefined && event.revision < lastRevision;
  const isDuplicate = event.revision === lastRevision;
  if (!staleRevision && !isDuplicate) lastRevision = event.revision;
  if (source) refreshHeartbeatTimeout();
  for (const observer of resourceObservers) observer(event);
  const active = new Set(aggregateResources().map(resourceKey));
  for (const resource of event.resources) {
    const key = resourceKey(resource);
    const version = {
      epoch: event.epoch,
      revision: event.revision,
    };
    if (active.has(key)) dispatchResource(resource, version);
    else rememberResourceVersion(resource, version);
    if (event.reason !== "change" && baselineReconciliations.delete(key))
      pendingResources.add(key);
  }
  pruneResourceVersions(active);
  const local = localResources().map(resourceKey);
  if (
    event.reason !== "change" &&
    !staleRevision &&
    !isDuplicate &&
    local.every((key) =>
      event.resources.some((resource) => resourceKey(resource) === key),
    )
  )
    pendingReset = true;
  setStatus("live");
  if (owner || independent) broadcast({ kind: "resource-event", event });
  scheduleFlush();
}

/** Observe accepted typed resource events without adding a stream subscription. */
export function observeResourceEvents(observer: ResourceObserver) {
  resourceObservers.add(observer);
  return () => resourceObservers.delete(observer);
}

function receiveTokenRateEvent(value: unknown, fromPeer = false) {
  if (!hasRevisionFrame(value)) throw new TypeError("Invalid token-rate frame");
  const event = value as ResourceTokenRatesEvent;
  if (event.workspaceId !== workspaceId) {
    setStatus("degraded");
    if (!fromPeer) reconnectNow();
    return;
  }
  if (fromPeer && (owner || independent)) return;
  if (!fromPeer) refreshHeartbeatTimeout();
  if (lastTokenEpoch !== event.epoch) {
    lastTokenEpoch = event.epoch;
    lastTokenRevision = undefined;
    lastTokenEvent = undefined;
  }
  if (lastTokenRevision !== undefined && event.revision <= lastTokenRevision)
    return;
  lastTokenRevision = event.revision;
  lastTokenEvent = event;
  setStatus("live");
  receiveResourceTokenRates(event);
  for (const listener of tokenRateListeners) listener(event);
  if (owner || independent) broadcast({ kind: "token-rates", event });
}

function scheduleFlush() {
  if (flushTimer !== undefined) return;
  flushTimer = setTimeout(function flushResourceChanges() {
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
      } else {
        for (const key of pendingResources) {
          const version = resourceValues.get(key);
          if (!version) continue;
          for (const listener of subscribers.get(key) || []) listener(version);
        }
        pendingResources = new Set();
      }
      retryCount = 0;
    } catch (error) {
      frameFailed(error);
    }
  }, RESOURCE_FLUSH_MS);
}

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
  dispatchEvent(event);
  scheduleFlush();
}

function acceptHeartbeat(value: unknown, fromPeer = false) {
  if (!hasRevisionFrame(value)) throw new TypeError("Invalid heartbeat frame");
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
      updateOwnerStream();
      replayResourceBaseline(added);
      if (
        record.tokenRates === true &&
        (!previous?.tokenRates || reset) &&
        lastTokenEvent
      )
        broadcast({ kind: "token-rates", event: lastTokenEvent });
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
  for (const { version, resources: known } of groups.values())
    broadcast({
      kind: "resource-event",
      event: {
        protocol: 3,
        workspaceId: workspaceId!,
        epoch: version.epoch,
        revision: version.revision,
        reason: "initial",
        resources: known,
      },
    });
}

function announceSubscriptions(reset = false) {
  if (!coordinatorReady || !workspaceId) return;
  broadcast({
    kind: "subscriptions",
    resources: localResources(),
    tokenRates: tokenRateListeners.size > 0,
    reset,
  });
  if (owner || independent) updateOwnerStream();
}

function closeSource() {
  clearTimeout(heartbeatTimeout);
  heartbeatTimeout = undefined;
  clearTimeout(schemaHandshakeTimeout);
  schemaHandshakeTimeout = undefined;
  source?.close();
  source = undefined;
  activeQueryKey = "";
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
    closeSource();
    return;
  }
  const queryKey = JSON.stringify([resources.map(resourceKey), tokenRates]);
  if (source && activeQueryKey === queryKey) return;
  if (source && currentStatus === "live") keepLiveOnReconfigure = true;
  closeSource();
  activeQueryKey = queryKey;
  if (!keepLiveOnReconfigure || currentStatus !== "live")
    setStatus("connecting");
  keepLiveOnReconfigure = false;
  try {
    const query = new URLSearchParams({
      protocol: "3",
      resources: JSON.stringify(resources),
      [API_SCHEMA_HASH_PARAM]: API_SCHEMA_HASH,
    });
    const connected = new EventSource(`/api/sync/stream?${query}`);
    source = connected;
    let schemaHandshakeReceived = false;
    let connectionOpened = false;
    const failHandshake = () => {
      if (source !== connected || schemaHandshakeReceived) return;
      requireBaselineReconciliation();
      closeSource();
      if (preHandshakeFailures === 0)
        preHandshakeResponseGeneration = matchingApiSchemaResponseGeneration();
      preHandshakeFailures++;
      if (preHandshakeFailures >= SCHEMA_HANDSHAKE_FAILURE_LIMIT) {
        if (
          preHandshakeResponseGeneration !== undefined &&
          matchingApiSchemaResponseGeneration() > preHandshakeResponseGeneration
        ) {
          scheduleReconnect();
        } else markApiSchemaMismatch();
      } else scheduleReconnect();
    };
    connected.onopen = () => {
      if (source !== connected) return;
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
    ) => {
      if (source !== connected) return;
      if (!schemaHandshakeReceived) {
        markApiSchemaMismatch();
        return;
      }
      try {
        apply(JSON.parse((message as MessageEvent<string>).data) as unknown);
        if (resetRetryOnSuccess) retryCount = 0;
      } catch (error) {
        frameFailed(error);
      }
    };
    connected.addEventListener("api-schema", (message: Event) => {
      if (source !== connected) return;
      try {
        const payload: unknown = JSON.parse(
          (message as MessageEvent<string>).data,
        );
        if (
          !payload ||
          typeof payload !== "object" ||
          (payload as { hash?: unknown }).hash !== API_SCHEMA_HASH ||
          (payload as Record<string, unknown>)[API_SCHEMA_MISMATCH_FIELD] ===
            true
        ) {
          markApiSchemaMismatch();
          return;
        }
        schemaHandshakeReceived = true;
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
      scheduleReconnect();
    };
    if (!heartbeatTimeout) {
      heartbeatTimeout = setTimeout(() => reconnectNow(), HEARTBEAT_TIMEOUT_MS);
    }
  } catch {
    closeSource();
    scheduleReconnect();
  }
}

function updateOwnerStream() {
  const resources = aggregateResources();
  const tokenRates = hasTokenRateInterest();
  const key = JSON.stringify([resources.map(resourceKey), tokenRates]);
  if (!resources.length && !tokenRates) {
    closeSource();
    return;
  }
  if (source && activeQueryKey !== key) {
    keepLiveOnReconfigure = currentStatus === "live";
    closeSource();
    openSource();
    return;
  }
  openSource();
}

function sendPeerHeartbeat() {
  if (!coordinatorReady || document.hidden || navigator.onLine === false)
    return;
  broadcast({ kind: "tab-heartbeat" });
  if (owner) {
    broadcast({ kind: "leader-heartbeat" });
    const now = Date.now();
    for (const [peerId, peer] of peerSubscriptions) {
      if (now - peer.seenAt > PEER_TIMEOUT_MS) peerSubscriptions.delete(peerId);
    }
    updateOwnerStream();
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
  openSource();
}

function startAsOwner() {
  if (!coordinatorActive || owner || independent || ownerRequestPending) return;
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
      `codex-sync-stream:${location.origin}:${requestedWorkspaceId}`,
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
        openSource();
        await new Promise<void>((resolve) => {
          releaseOwner = resolve;
        });
        releaseOwner = undefined;
        owner = false;
        closeSource();
        if (peerHeartbeatTimer !== undefined) clearInterval(peerHeartbeatTimer);
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
          `codex-sync-${location.origin}-${workspaceId}`,
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
  if (owner || independent) openSource();
  announceSubscriptions(true);
}

function stopCoordinator() {
  coordinatorActive = false;
  coordinatorGeneration++;
  initializing = false;
  releaseStream();
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
  resourceRefs.clear();
  peerSubscriptions.clear();
  setStatus(isApiSchemaMismatch() ? "schema-mismatch" : "connecting");
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

export function watchResourceChanges(
  resource: ResourceRef,
  callback: (version?: ResourceVersion) => void,
): () => void {
  const key = resourceKey(resource);
  let listeners = subscribers.get(key);
  if (!listeners) subscribers.set(key, (listeners = new Set()));
  resourceRefs.set(key, resource);
  listeners.add(callback);
  const known = resourceValues.get(key);
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
    if (!listeners?.size) {
      subscribers.delete(key);
      resourceRefs.delete(key);
      baselineReconciliations.delete(key);
    }
    announceSubscriptions();
    if (
      !subscribers.size &&
      !tokenRateListeners.size &&
      (!owner || !aggregateResources().length)
    )
      stopCoordinator();
  };
}

export function watchResourceConnection(
  listener: (status: ResourceConnectionState) => void,
): () => void {
  transportStatusListeners.add(listener);
  listener(currentStatus);
  return () => transportStatusListeners.delete(listener);
}

export function watchTokenRateEvents(
  listener: (event: ResourceTokenRatesEvent) => void,
): () => void {
  tokenRateListeners.add(listener);
  if (lastTokenEvent && lastTokenEvent.workspaceId === workspaceId)
    listener(lastTokenEvent);
  startCoordinator();
  announceSubscriptions();
  let stopped = false;
  return () => {
    if (stopped) return;
    stopped = true;
    tokenRateListeners.delete(listener);
    announceSubscriptions();
    if (!subscribers.size && !tokenRateListeners.size) stopCoordinator();
  };
}

configureTokenRateStream(watchTokenRateEvents);
onApiSchemaMismatch(stopCoordinator);

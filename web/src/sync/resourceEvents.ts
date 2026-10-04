import {
  isResourceChangeEvent,
  isResourceHeartbeatEvent,
  isResourceRef,
  isResourceTokenRatesEvent,
} from "../generated/stream-validators.js";
import type { components } from "../generated/api";
import { syncDatabase } from "./client";
import { onResume } from "./resume";
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

type Version = { epoch: string; revision: number };
export type ResourceConnectionState =
  | "connecting"
  | "live"
  | "degraded"
  | "offline";
type Listener = () => void;
type TabSubscriptions = {
  kind: "subscriptions";
  workspaceId: string;
  tabId: string;
  resources: unknown;
  tokenRates: unknown;
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
type TabMessage =
  | TabSubscriptions
  | TabEvent
  | TabTokenRates
  | TabHeartbeat
  | LeaderHeartbeat
  | TabStatus;
type OutgoingMessage =
  | { kind: "subscriptions"; resources: ResourceRef[]; tokenRates: boolean }
  | { kind: "resource-event"; event: ResourceChangeEvent }
  | { kind: "token-rates"; event: ResourceTokenRatesEvent }
  | { kind: "tab-heartbeat" }
  | { kind: "leader-heartbeat" }
  | { kind: "status"; status: ResourceConnectionState };

const HEARTBEAT_TIMEOUT_MS = 45_000;
const PEER_HEARTBEAT_MS = 3_000;
const PEER_TIMEOUT_MS = 10_000;
const BASE_RETRY_MS = 500;
const MAX_RETRY_MS = 15_000;
const RESOURCE_FLUSH_MS = 20;

const subscribers = new Map<string, Set<Listener>>();
const resourceRefs = new Map<string, ResourceRef>();
const resourceValues = new Map<string, Version>();
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
let releaseOwner: (() => void) | undefined;
let peerHeartbeatTimer: ReturnType<typeof setInterval> | undefined;
let heartbeatTimeout: ReturnType<typeof setTimeout> | undefined;
let reconnectTimer: ReturnType<typeof setTimeout> | undefined;
let flushTimer: ReturnType<typeof setTimeout> | undefined;
let retryCount = 0;
let lastEpoch: string | undefined;
let lastRevision: number | undefined;
let lastHeartbeatRevision: number | undefined;
let lastTokenEpoch: string | undefined;
let lastTokenRevision: number | undefined;
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
  const key = resourceKey(resource);
  const previous = resourceValues.get(key);
  if (
    previous?.epoch === version.epoch &&
    previous.revision === version.revision
  )
    return;
  resourceValues.set(key, version);
  pendingResources.add(key);
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
    resourceValues.clear();
  }
  if (lastRevision !== undefined && event.revision < lastRevision) return;
  const isDuplicate = event.revision === lastRevision;
  if (!isDuplicate) lastRevision = event.revision;
  const active = new Set(aggregateResources().map(resourceKey));
  for (const resource of event.resources) {
    if (!active.has(resourceKey(resource))) continue;
    dispatchResource(resource, {
      epoch: event.epoch,
      revision: event.revision,
    });
  }
  if (event.reason !== "change" && !isDuplicate) pendingReset = true;
  setStatus("live");
  if (owner || independent) broadcast({ kind: "resource-event", event });
  scheduleFlush();
}

function receiveTokenRateEvent(value: unknown, fromPeer = false) {
  if (!isResourceTokenRatesEvent(value)) {
    setStatus("degraded");
    if (!fromPeer) reconnectNow();
    return;
  }
  if (value.workspaceId !== workspaceId) {
    setStatus("degraded");
    if (!fromPeer) reconnectNow();
    return;
  }
  if (fromPeer && (owner || independent)) return;
  if (lastTokenEpoch !== value.epoch) {
    lastTokenEpoch = value.epoch;
    lastTokenRevision = undefined;
  }
  if (lastTokenRevision !== undefined && value.revision <= lastTokenRevision)
    return;
  lastTokenRevision = value.revision;
  setStatus("live");
  receiveResourceTokenRates(value);
  for (const listener of tokenRateListeners) listener(value);
  if (owner || independent) broadcast({ kind: "token-rates", event: value });
}

function scheduleFlush() {
  if (flushTimer !== undefined) return;
  flushTimer = setTimeout(() => {
    flushTimer = undefined;
    if (pendingReset) {
      pendingReset = false;
      const subscribed = new Set(aggregateResources().map(resourceKey));
      for (const [key, listeners] of subscribers) {
        if (subscribed.has(key)) for (const listener of listeners) listener();
      }
      pendingResources = new Set();
      return;
    }
    for (const key of pendingResources) {
      const version = resourceValues.get(key);
      if (!version) continue;
      for (const listener of subscribers.get(key) || []) listener();
    }
    pendingResources = new Set();
  }, RESOURCE_FLUSH_MS);
}

function receiveResourceEvent(value: unknown, fromPeer = false) {
  if (!isResourceChangeEvent(value)) {
    setStatus("degraded");
    if (!fromPeer) reconnectNow();
    return;
  }
  if (fromPeer && (owner || independent)) return;
  if (!workspaceId || value.workspaceId !== workspaceId) {
    setStatus("degraded");
    if (!fromPeer) reconnectNow();
    return;
  }
  dispatchEvent(value);
  scheduleFlush();
}

function acceptHeartbeat(value: unknown, fromPeer = false) {
  if (!isResourceHeartbeatEvent(value)) {
    setStatus("degraded");
    if (!fromPeer) reconnectNow();
    return;
  }
  if (value.workspaceId !== workspaceId) {
    setStatus("degraded");
    if (!fromPeer) reconnectNow();
    return;
  }
  if (lastEpoch && value.epoch !== lastEpoch) {
    lastEpoch = undefined;
    lastRevision = undefined;
    lastHeartbeatRevision = undefined;
    reconnectNow();
    return;
  }
  if (
    (lastRevision !== undefined && value.revision < lastRevision) ||
    (lastHeartbeatRevision !== undefined &&
      value.revision < lastHeartbeatRevision)
  ) {
    reconnectNow();
    return;
  }
  if (fromPeer) return;
  lastHeartbeatRevision = value.revision;
  clearTimeout(heartbeatTimeout);
  heartbeatTimeout = setTimeout(() => reconnectNow(), HEARTBEAT_TIMEOUT_MS);
  setStatus("live");
  if (owner) broadcast({ kind: "leader-heartbeat" });
}

function receiveChannelMessage(value: unknown) {
  if (!value || typeof value !== "object" || Array.isArray(value)) return;
  const record = value as Record<string, unknown>;
  if (
    record.workspaceId !== workspaceId ||
    typeof record.tabId !== "string" ||
    !record.tabId ||
    record.tabId === tabId ||
    typeof record.kind !== "string"
  )
    return;
  if (record.kind === "subscriptions") {
    if (
      !Array.isArray(record.resources) ||
      !record.resources.every(isResourceRef)
    )
      return;
    peerSubscriptions.set(record.tabId, {
      resources: record.resources,
      tokenRates: record.tokenRates === true,
      seenAt: Date.now(),
    });
    if (owner) updateOwnerStream();
  } else if (record.kind === "tab-heartbeat") {
    const peer = peerSubscriptions.get(record.tabId);
    if (peer) peer.seenAt = Date.now();
  } else if (record.kind === "leader-heartbeat") {
    lastLeaderHeartbeatAt = Date.now();
    const peer = peerSubscriptions.get(record.tabId);
    if (peer) peer.seenAt = Date.now();
  } else if (record.kind === "resource-event") {
    receiveResourceEvent(record.event, true);
  } else if (record.kind === "token-rates") {
    receiveTokenRateEvent(record.event, true);
  } else if (
    record.kind === "status" &&
    (record.status === "connecting" ||
      record.status === "live" ||
      record.status === "degraded" ||
      record.status === "offline")
  ) {
    if (!owner && !independent) setStatus(record.status);
  }
}

function announceSubscriptions() {
  if (!coordinatorReady || !workspaceId) return;
  broadcast({
    kind: "subscriptions",
    resources: localResources(),
    tokenRates: tokenRateListeners.size > 0,
  });
  if (owner || independent) updateOwnerStream();
}

function closeSource() {
  clearTimeout(heartbeatTimeout);
  heartbeatTimeout = undefined;
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
    if (owner || independent) openSource();
  }, retryDelay());
}

function reconnectNow() {
  closeSource();
  scheduleReconnect();
}

function openSource() {
  if (
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
    });
    const connected = new EventSource(`/api/sync/stream?${query}`);
    source = connected;
    connected.addEventListener("resources", (message: Event) => {
      if (source !== connected) return;
      let parsed: unknown;
      try {
        parsed = JSON.parse((message as MessageEvent<string>).data) as unknown;
      } catch {
        setStatus("degraded");
        reconnectNow();
        return;
      }
      retryCount = 0;
      receiveResourceEvent(parsed);
    });
    connected.addEventListener("heartbeat", (message: Event) => {
      if (source !== connected) return;
      let parsed: unknown;
      try {
        parsed = JSON.parse((message as MessageEvent<string>).data) as unknown;
      } catch {
        reconnectNow();
        return;
      }
      acceptHeartbeat(parsed);
    });
    connected.addEventListener("token-rates", (message: Event) => {
      if (source !== connected) return;
      let parsed: unknown;
      try {
        parsed = JSON.parse((message as MessageEvent<string>).data) as unknown;
      } catch {
        setStatus("degraded");
        reconnectNow();
        return;
      }
      receiveTokenRateEvent(parsed);
    });
    connected.onerror = () => {
      if (source !== connected) return;
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
  broadcast({
    kind: "subscriptions",
    resources: localResources(),
    tokenRates: tokenRateListeners.size > 0,
  });
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
  ownerRequestPending = true;
  void locks
    .request(
      `codex-sync-stream:${location.origin}:${workspaceId}`,
      { mode: "exclusive", ifAvailable: true },
      async (lock) => {
        ownerRequestPending = false;
        if (
          !coordinatorActive ||
          !lock ||
          document.hidden ||
          navigator.onLine === false
        )
          return;
        owner = true;
        startPeerHeartbeat();
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
    .catch(() => {
      ownerRequestPending = false;
      startIndependent();
    });
}

function releaseStream() {
  const release = releaseOwner;
  releaseOwner = undefined;
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
        channel.onmessage = (event: MessageEvent<unknown>) =>
          receiveChannelMessage(event.data);
      }
    } catch {
      channel = undefined;
    }
    startPeerHeartbeat();
    announceSubscriptions();
    if (!channel || !navigator.locks) startIndependent();
    else startAsOwner();
  } catch {
    if (generation === coordinatorGeneration)
      setStatus(navigator.onLine === false ? "offline" : "degraded");
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
  else if (!owner && !independent) startAsOwner();
  else {
    closeSource();
    openSource();
  }
  announceSubscriptions();
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
  resourceValues.clear();
  resourceRefs.clear();
  peerSubscriptions.clear();
  setStatus("connecting");
}

function startCoordinator() {
  if (coordinatorActive) return;
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
      broadcast({ kind: "subscriptions", resources: [], tokenRates: false });
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
  callback: () => void,
): () => void {
  const key = resourceKey(resource);
  let listeners = subscribers.get(key);
  if (!listeners) subscribers.set(key, (listeners = new Set()));
  resourceRefs.set(key, resource);
  listeners.add(callback);
  const known = resourceValues.get(key);
  if (known) callback();
  startCoordinator();
  announceSubscriptions();
  let stopped = false;
  return () => {
    if (stopped) return;
    stopped = true;
    listeners?.delete(callback);
    if (!listeners?.size) {
      subscribers.delete(key);
      resourceRefs.delete(key);
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

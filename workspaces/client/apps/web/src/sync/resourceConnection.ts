import { API_SCHEMA_HASH } from "../generated/apiSchema";
import { hasNativeClient } from "../native/client";
import type { ConnectionHealth, ReconnectCause } from "./connectionState";

/**
 * Whether the sync transport treats the page as hidden. The desktop app keeps
 * its stream while its window is in the background: a reopen on every window
 * switch replays the initial sync through the native bridge.
 */
export function pageHidden() {
  return document.hidden && !hasNativeClient();
}

/** Browser peer health protocol version; incompatible renderers own separate leases. */
export const RESOURCE_COORDINATION_VERSION = 2;
export function resourceCoordinationNames(origin: string, workspaceId: string) {
  const scope = `${RESOURCE_COORDINATION_VERSION}:${API_SCHEMA_HASH}:${origin}:${workspaceId}`;
  return { channel: `codex-sync:${scope}`, lock: `codex-sync-stream:${scope}` };
}

/** Maximum retained owner generations, in tab identities. */
const MAX_PEER_OWNER_HISTORY = 128;

/** Current local recovery work, independently of validated server health. */

/** Observed failure boundaries; browsers do not expose a stream error's cause. */

export type ResourceConnectionDetail = {
  failure?: ReconnectCause | undefined;
  /** Deadline of the current scheduled retry, in Unix epoch milliseconds. */
  retryAt?: number | undefined;
  attempt?: number | undefined;
};

/** Maximum wait for optional local resume cursors, in milliseconds. */
export const RESOURCE_CURSOR_PREPARATION_TIMEOUT_MS = 5_000;

/** First wait before a stream retry, in milliseconds. */
export const RESOURCE_RETRY_BASE_MS = 500;
/** Longest wait between stream retries, in milliseconds. */
export const RESOURCE_RETRY_MAX_MS = 5 * 60_000;
/** How long a stream must stay up before the retry count starts again, in milliseconds. */
export const RESOURCE_STABLE_MS = 60_000;
/** Longest wait for the answer to a probe of an open stream, in milliseconds. */
export const RESOURCE_PROBE_TIMEOUT_MS = 5_000;

/** Maximum silence after a validated server frame, in milliseconds. */
export const RESOURCE_HEARTBEAT_TIMEOUT_MS = 45_000;
/** Maximum silence/publication age for an owner announcement, in milliseconds. */
export const RESOURCE_PEER_TIMEOUT_MS = 10_000;

export function isResourceConnectionState(
  value: unknown,
): value is ConnectionHealth {
  return (
    value === "connecting" ||
    value === "live" ||
    value === "degraded" ||
    value === "offline" ||
    value === "paused" ||
    value === "schema-mismatch"
  );
}

export type ResourceConnectionHealth = {
  status: ConnectionHealth;
  /** Monotonic source generation within one tab identity. */
  generation: number;
  /** Monotonic publication sequence within one tab identity. */
  sequence: number;
  sentAt: number;
  /** Last validated server frame time; peer publications never advance it. */
  validatedAt?: number | undefined;
};

export function hasFreshServerFrame(
  validatedAt: number | undefined,
  now = Date.now(),
) {
  return (
    validatedAt !== undefined &&
    validatedAt <= now &&
    now - validatedAt < RESOURCE_HEARTBEAT_TIMEOUT_MS
  );
}

/** Call only after the outer workspace/schema envelope has been checked. */
export function createPeerConnectionHealth() {
  let current: { tabId: string; health: ResourceConnectionHealth } | undefined;
  const owners = new Map<string, ResourceConnectionHealth>();
  return {
    clear() {
      current = undefined;
      owners.clear();
    },
    status(now = Date.now()): ConnectionHealth | undefined {
      if (!current) return;
      const { health } = current;
      if (now - health.sentAt > RESOURCE_PEER_TIMEOUT_MS) return "degraded";
      if (health.status === "paused" || health.status === "offline")
        return "degraded";
      if (health.status !== "live") return health.status;
      return hasFreshServerFrame(health.validatedAt, now) ? "live" : "degraded";
    },
    accept(tabId: string, value: unknown, now = Date.now()): boolean {
      if (!value || typeof value !== "object") return false;
      const health = value as ResourceConnectionHealth;
      if (
        !isResourceConnectionState(health.status) ||
        !Number.isSafeInteger(health.generation) ||
        health.generation < 0 ||
        !Number.isSafeInteger(health.sequence) ||
        health.sequence < 0 ||
        !Number.isFinite(health.sentAt) ||
        health.sentAt > now ||
        now - health.sentAt > RESOURCE_PEER_TIMEOUT_MS ||
        (health.validatedAt !== undefined &&
          (!Number.isFinite(health.validatedAt) ||
            health.validatedAt > health.sentAt))
      )
        return false;
      const previous = owners.get(tabId);
      if (
        previous &&
        (health.generation < previous.generation ||
          health.sequence <= previous.sequence)
      )
        return false;
      if (current) {
        if (health.sentAt < current.health.sentAt) return false;
        // A former owner must acquire a new source generation before returning.
        if (
          current.tabId !== tabId &&
          previous &&
          health.generation <= previous.generation
        )
          return false;
      }
      owners.delete(tabId);
      owners.set(tabId, health);
      if (owners.size > MAX_PEER_OWNER_HISTORY)
        owners.delete(owners.keys().next().value!);
      current = { tabId, health };
      return true;
    },
  };
}

/** Each lifecycle event reaches the transport once. */
export function onResourceConnectionLifecycle(actions: {
  resume: () => void;
  online: () => void;
  offline: () => void;
  pause: () => void;
  /** The kind or quality of the network changed while the device stayed online. */
  change: () => void;
}) {
  const visibility = () => {
    if (pageHidden()) actions.pause();
    else actions.resume();
  };
  window.addEventListener("pageshow", actions.resume);
  window.addEventListener("offline", actions.offline);
  window.addEventListener("online", actions.online);
  document.addEventListener("visibilitychange", visibility);
  const connection = (
    navigator as {
      connection?: Pick<
        EventTarget,
        "addEventListener" | "removeEventListener"
      >;
    }
  ).connection;
  connection?.addEventListener("change", actions.change);
  return () => {
    connection?.removeEventListener("change", actions.change);
    window.removeEventListener("pageshow", actions.resume);
    window.removeEventListener("offline", actions.offline);
    window.removeEventListener("online", actions.online);
    document.removeEventListener("visibilitychange", visibility);
  };
}

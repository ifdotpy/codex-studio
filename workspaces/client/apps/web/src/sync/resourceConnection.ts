import { onResume } from "./resume";
import { API_SCHEMA_HASH } from "../generated/apiSchema";

/** Browser peer health protocol version; incompatible renderers own separate leases. */
export const RESOURCE_COORDINATION_VERSION = 2;
export function resourceCoordinationNames(origin: string, workspaceId: string) {
  const scope = `${RESOURCE_COORDINATION_VERSION}:${API_SCHEMA_HASH}:${origin}:${workspaceId}`;
  return { channel: `codex-sync:${scope}`, lock: `codex-sync-stream:${scope}` };
}

/** Maximum retained owner generations, in tab identities. */
const MAX_PEER_OWNER_HISTORY = 128;

export type ResourceConnectionState =
  | "connecting"
  | "live"
  | "degraded"
  | "offline"
  | "paused"
  | "schema-mismatch";
/** Current local recovery work, independently of validated server health. */
export type ResourceStreamState =
  | "stopped"
  | "preparing"
  | "waiting-owner"
  | "retry-scheduled"
  | "connecting"
  | "open"
  | "paused"
  | "offline";

/** Observed failure boundaries; browsers do not expose a stream error's cause. */
export type ResourceConnectionFailure =
  | "stream-error"
  | "heartbeat-timeout"
  | "schema-handshake-timeout"
  | "subscription-timeout"
  | "subscription-rejected"
  | "invalid-frame"
  | "local-state-error"
  | "unknown";

export type ResourceConnectionDetail = {
  failure?: ResourceConnectionFailure | undefined;
  /** Deadline of the current scheduled retry, in Unix epoch milliseconds. */
  retryAt?: number | undefined;
};

/** Maximum wait for optional local resume cursors, in milliseconds. */
export const RESOURCE_CURSOR_PREPARATION_TIMEOUT_MS = 5_000;

/** Maximum silence after a validated server frame, in milliseconds. */
export const RESOURCE_HEARTBEAT_TIMEOUT_MS = 45_000;
/** Maximum silence/publication age for an owner announcement, in milliseconds. */
export const RESOURCE_PEER_TIMEOUT_MS = 10_000;

export function isResourceConnectionState(
  value: unknown,
): value is ResourceConnectionState {
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
  status: ResourceConnectionState;
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
    status(now = Date.now()): ResourceConnectionState | undefined {
      if (!current) return;
      const { health } = current;
      if (now - health.sentAt > RESOURCE_PEER_TIMEOUT_MS) return "degraded";
      if (health.status === "paused") return "degraded";
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

/** Resume owns online/pageshow/visible coalescing; this registration owns pauses. */
export function onResourceConnectionLifecycle(actions: {
  resume: () => void;
  offline: () => void;
  pause: () => void;
}) {
  const stopResume = onResume(actions.resume);
  const visibility = () => {
    if (document.hidden) actions.pause();
  };
  window.addEventListener("offline", actions.offline);
  document.addEventListener("visibilitychange", visibility);
  return () => {
    stopResume();
    window.removeEventListener("offline", actions.offline);
    document.removeEventListener("visibilitychange", visibility);
  };
}

/** A transport failure or an explicit change in reachability. */
export type ReconnectCause =
  | "stream-error"
  | "heartbeat-timeout"
  | "schema-handshake-timeout"
  | "subscription-timeout"
  | "subscription-rejected"
  | "invalid-frame"
  | "local-state-error"
  | "network-online"
  | "probe-timeout"
  | "route-change"
  | "auth-rejected"
  | "server-restart"
  | "unknown";

export const reconnectReasons: Record<ReconnectCause, string> = {
  "stream-error": "Connection failed.",
  "heartbeat-timeout": "Server updates timed out.",
  "schema-handshake-timeout": "Server compatibility check timed out.",
  "subscription-timeout": "Server updates did not start in time.",
  "subscription-rejected": "Could not confirm live updates.",
  "invalid-frame": "Received invalid server updates.",
  "local-state-error": "Could not prepare or apply local sync data.",
  "network-online": "The device is online again.",
  "probe-timeout": "The server did not answer a connection check.",
  "route-change": "A faster route to the server is available.",
  "auth-rejected":
    "The server refused this device. Retry after you check access.",
  "server-restart": "The server restarted.",
  unknown: "Reason unknown.",
};

export type ConnectingActivity =
  "stopped" | "preparing" | "waiting-owner" | "connecting" | "open";
export type ReconnectingActivity = ConnectingActivity | "retry-scheduled";

export type ConnectionState =
  | { phase: "stopped" }
  | { phase: "connecting"; attempt: number; activity: ConnectingActivity }
  | { phase: "live"; since: number }
  | {
      phase: "reconnecting";
      attempt: number;
      cause: ReconnectCause;
      retryAt?: number | undefined;
      activity: ReconnectingActivity;
    }
  | { phase: "offline"; since: number }
  | { phase: "paused" }
  | { phase: "schema-mismatch" };

/** Coarse health is the existing cross-tab wire representation. */
export type ConnectionHealth =
  "connecting" | "live" | "degraded" | "offline" | "paused" | "schema-mismatch";
export type ConnectionActivity =
  | ReconnectingActivity
  /** The open stream waits for a local write that outlasted its deadline. */
  | "storage-slow"
  | "paused"
  | "offline";

export function healthOf(state: ConnectionState): ConnectionHealth {
  switch (state.phase) {
    case "stopped":
    case "connecting":
      return "connecting";
    case "reconnecting":
      return "degraded";
    case "live":
    case "offline":
    case "paused":
    case "schema-mismatch":
      return state.phase;
    default: {
      const exhaustive: never = state;
      return exhaustive;
    }
  }
}

export function activityOf(state: ConnectionState): ConnectionActivity {
  switch (state.phase) {
    case "connecting":
    case "reconnecting":
      return state.activity;
    case "live":
      return "open";
    case "offline":
    case "paused":
      return state.phase;
    case "stopped":
    case "schema-mismatch":
      return "stopped";
    default: {
      const exhaustive: never = state;
      return exhaustive;
    }
  }
}

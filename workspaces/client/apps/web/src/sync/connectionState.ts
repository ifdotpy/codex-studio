/** A transport failure or an explicit change in reachability. */
export type ReconnectCause =
  | "stream-error"
  | "heartbeat-timeout"
  | "schema-handshake-timeout"
  | "subscription-timeout"
  | "subscription-rejected"
  | "invalid-frame"
  | "local-state-error"
  | "network-offline"
  | "network-online"
  | "page-resumed"
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
  "network-offline": "The device went offline.",
  "network-online": "The device is online again.",
  "page-resumed": "The page resumed.",
  "server-restart": "The server restarted.",
  unknown: "Reason unknown.",
};

export type ConnectingActivity =
  | "stopped"
  | "preparing"
  | "waiting-owner"
  | "connecting"
  | "open";
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

export type ConnectionEvent =
  | { type: "stopped" }
  | { type: "connecting"; attempt: number; activity: ConnectingActivity }
  | { type: "live"; at: number }
  | {
      type: "reconnecting";
      attempt: number;
      cause: ReconnectCause;
      retryAt?: number | undefined;
      activity: ReconnectingActivity;
    }
  | { type: "offline"; at: number }
  | { type: "paused" }
  | { type: "schema-mismatch" };

export function transitionConnection(
  state: ConnectionState,
  event: ConnectionEvent,
): ConnectionState {
  switch (event.type) {
    case "stopped":
      return { phase: "stopped" };
    case "connecting":
      return {
        phase: "connecting",
        attempt: event.attempt,
        activity: event.activity,
      };
    case "live":
      return state.phase === "live"
        ? state
        : { phase: "live", since: event.at };
    case "reconnecting":
      return {
        phase: "reconnecting",
        attempt: event.attempt,
        cause: event.cause,
        activity: event.activity,
        ...(event.retryAt === undefined ? {} : { retryAt: event.retryAt }),
      };
    case "offline":
      return state.phase === "offline"
        ? state
        : { phase: "offline", since: event.at };
    case "paused":
      return { phase: "paused" };
    case "schema-mismatch":
      return { phase: "schema-mismatch" };
    default: {
      const exhaustive: never = event;
      return exhaustive;
    }
  }
}

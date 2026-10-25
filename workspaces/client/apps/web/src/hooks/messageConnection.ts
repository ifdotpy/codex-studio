import type {
  ResourceConnectionDetail,
  ResourceConnectionFailure,
  ResourceConnectionState,
  ResourceStreamState,
} from "../sync/resourceConnection";
import { localTime } from "../local-time";

const failureLabels: Record<ResourceConnectionFailure, string> = {
  "stream-error": "Connection failed.",
  "heartbeat-timeout": "Server updates timed out.",
  "schema-handshake-timeout": "Server compatibility check timed out.",
  "subscription-timeout": "Server updates did not start in time.",
  "subscription-rejected": "Could not confirm live updates.",
  "invalid-frame": "Received invalid server updates.",
  "local-state-error": "Could not apply server updates on this device.",
  unknown: "Reason unknown.",
};
const activityLabels: Record<ResourceStreamState, string> = {
  stopped: "Connection interrupted.",
  preparing: "Preparing connection…",
  "waiting-owner": "Waiting for connection…",
  "retry-scheduled": "Retry scheduled.",
  connecting: "Connecting…",
  open: "Waiting for server updates…",
  paused: "",
  offline: "No network.",
};

export type MessageConnection = {
  health: ResourceConnectionState;
  activity: ResourceStreamState | undefined;
  message: string;
  icon: "waiting" | "connecting" | "offline";
};

/** Present transport facts without turning unhealthy data into a retry claim. */
export function selectMessageConnection(
  health: ResourceConnectionState,
  activity?: ResourceStreamState,
  detail: ResourceConnectionDetail = {},
): MessageConnection {
  const connection: MessageConnection = {
    health,
    activity,
    message: "",
    icon: "waiting",
  };
  // The network hint governs visible copy, independently of server freshness.
  if (activity === "offline")
    return { ...connection, message: activityLabels.offline, icon: "offline" };
  if (
    health === "live" ||
    health === "paused" ||
    health === "schema-mismatch" ||
    activity === "paused"
  )
    return connection;

  const interrupted = health === "degraded" || health === "offline";
  let action = activityLabels[activity ?? "stopped"];
  if (activity === "connecting") {
    connection.icon = "connecting";
    if (interrupted) action = "Reconnecting…";
  } else if (activity === "stopped" || activity === undefined)
    connection.icon = "offline";
  else if (activity === "retry-scheduled" && detail.retryAt !== undefined)
    action = `Retry scheduled for ${localTime(new Date(detail.retryAt), {
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
    })}.`;

  const reason = detail.failure
    ? failureLabels[detail.failure]
    : activity === "stopped" ||
        activity === undefined ||
        activity === "retry-scheduled" ||
        (interrupted &&
          activity !== "preparing" &&
          activity !== "waiting-owner")
      ? failureLabels.unknown
      : "";
  connection.message = reason ? `${reason} ${action}` : action;
  return connection;
}

import type {
  ConnectionActivity,
  ConnectionHealth,
} from "../sync/connectionState";
import type { ResourceConnectionDetail } from "../sync/resourceConnection";
import { localTime } from "../local-time";
import { reconnectReasons } from "../sync/connectionState";

const activityLabels: Record<ConnectionActivity, string> = {
  stopped: "Connection interrupted.",
  preparing: "Preparing connection…",
  "waiting-owner": "Waiting for the shared connection…",
  "retry-scheduled": "Retry scheduled.",
  connecting: "Connecting…",
  open: "Waiting for server updates…",
  paused: "",
  offline: "No network.",
};

export type MessageConnection = {
  health: ConnectionHealth;
  activity: ConnectionActivity | undefined;
  message: string;
  icon: "waiting" | "connecting" | "offline";
};

/** Present transport facts without turning unhealthy data into a retry claim. */
export function selectMessageConnection(
  health: ConnectionHealth,
  activity?: ConnectionActivity,
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
    ? reconnectReasons[detail.failure]
    : activity === "stopped" ||
        activity === undefined ||
        activity === "retry-scheduled" ||
        (interrupted &&
          activity !== "preparing" &&
          activity !== "waiting-owner")
      ? reconnectReasons.unknown
      : "";
  const attempt =
    detail.attempt === undefined ? "" : ` Attempt ${detail.attempt}.`;
  connection.message = `${reason ? `${reason} ${action}` : action}${attempt}`;
  return connection;
}

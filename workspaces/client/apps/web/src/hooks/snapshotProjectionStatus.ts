export type SnapshotProjectionStatus = {
  startupError: string;
  projectionError: string;
  transportError: string;
};

export const initialSnapshotProjectionStatus: SnapshotProjectionStatus = {
  startupError: "",
  projectionError: "",
  transportError: "",
};

export type SnapshotProjectionStatusAction =
  | { type: "startup-failed"; error: string }
  | { type: "startup-recovered" }
  | { type: "projection-failed"; error: string }
  | { type: "projection-recovered" }
  | { type: "data-received" }
  | { type: "transport-changed"; error: string };

// A repeated report must keep the state identity so it schedules no render.
function withStatus(
  status: SnapshotProjectionStatus,
  change: Partial<SnapshotProjectionStatus>,
): SnapshotProjectionStatus {
  const next = { ...status, ...change };
  return next.startupError === status.startupError &&
    next.projectionError === status.projectionError &&
    next.transportError === status.transportError
    ? status
    : next;
}

export function snapshotProjectionStatusReducer(
  status: SnapshotProjectionStatus,
  action: SnapshotProjectionStatusAction,
): SnapshotProjectionStatus {
  switch (action.type) {
    case "startup-failed":
      return withStatus(status, { startupError: action.error });
    case "startup-recovered":
      return withStatus(status, { startupError: "" });
    case "projection-failed":
      return withStatus(status, { projectionError: action.error });
    case "projection-recovered":
      return withStatus(status, { projectionError: "" });
    case "data-received":
      return withStatus(status, { startupError: "", projectionError: "" });
    case "transport-changed":
      return withStatus(status, { transportError: action.error });
  }
}

export function snapshotProjectionError(
  status: SnapshotProjectionStatus,
): string {
  return status.startupError || status.projectionError || status.transportError;
}

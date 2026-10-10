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

export function snapshotProjectionStatusReducer(
  status: SnapshotProjectionStatus,
  action: SnapshotProjectionStatusAction,
): SnapshotProjectionStatus {
  switch (action.type) {
    case "startup-failed":
      return { ...status, startupError: action.error };
    case "startup-recovered":
      return { ...status, startupError: "" };
    case "projection-failed":
      return { ...status, projectionError: action.error };
    case "projection-recovered":
      return { ...status, projectionError: "" };
    case "data-received":
      return { ...status, startupError: "", projectionError: "" };
    case "transport-changed":
      return { ...status, transportError: action.error };
  }
}

export function snapshotProjectionError(
  status: SnapshotProjectionStatus,
): string {
  return status.startupError || status.projectionError || status.transportError;
}

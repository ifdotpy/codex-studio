import { describe, expect, it } from "vitest";
import {
  initialSnapshotProjectionStatus,
  snapshotProjectionError,
  snapshotProjectionStatusReducer,
} from "./snapshotProjectionStatus";

describe("snapshot projection status", () => {
  it("shows the startup alert after an initial projection failure and clears on data", () => {
    const failed = snapshotProjectionStatusReducer(
      initialSnapshotProjectionStatus,
      { type: "startup-failed", error: "Workspace identity unavailable" },
    );
    expect(snapshotProjectionError(failed)).toBe(
      "Workspace identity unavailable",
    );

    const recovered = snapshotProjectionStatusReducer(failed, {
      type: "data-received",
    });
    expect(snapshotProjectionError(recovered)).toBe("");
  });

  it("shows a later subscription error and clears it after a successful pull", () => {
    const failed = snapshotProjectionStatusReducer(
      initialSnapshotProjectionStatus,
      { type: "projection-failed", error: "Entity pull failed" },
    );
    expect(snapshotProjectionError(failed)).toBe("Entity pull failed");

    const recovered = snapshotProjectionStatusReducer(failed, {
      type: "projection-recovered",
    });
    expect(snapshotProjectionError(recovered)).toBe("");
  });
});

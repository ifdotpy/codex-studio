import { describe, expect, it } from "vitest";
import {
  isEntityResetResponse,
  requiredSyncNumber,
  type SyncPullResponse,
} from "./pullContract";

const reset: SyncPullResponse = {
  floor: 7,
  generation: 2,
  maxSeq: 7,
  reset: true,
  workspaceId: "workspace-1",
};

describe("sync pull response contract", () => {
  it("treats reset as a distinct entity-scope response", () => {
    expect(isEntityResetResponse(reset, "state:entities:v1")).toBe(true);
    expect(() => isEntityResetResponse(reset, "transcript:agent-1")).toThrow(
      "unsupported sync scope",
    );
  });

  it("rejects absent, fractional, negative, and unsafe checkpoints", () => {
    expect(requiredSyncNumber(0, "checkpoint")).toBe(0);
    for (const value of [undefined, null, -1, 1.5, Number.MAX_SAFE_INTEGER + 1])
      expect(() => requiredSyncNumber(value, "checkpoint")).toThrow(
        "invalid checkpoint",
      );
  });
});

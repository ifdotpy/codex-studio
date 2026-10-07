import { describe, expect, it, vi } from "vitest";
import {
  EntitySequenceCheckpoint,
  entitySequenceInvalidationCovered,
  getEntitySequenceCheckpoint,
  getEntitySequenceCheckpointForWorkspace,
  setEntitySequenceProjection,
} from "./entitySequence";

describe("entity sequence checkpoint", () => {
  it("requires contiguous coverage for in-flight, gate, and persister advancement", async () => {
    const checkpoint = new EntitySequenceCheckpoint();
    checkpoint.assign(100);
    const resetVersion = checkpoint.resetVersion;
    const token = checkpoint.beginInFlightCoverage(100, 102)!;

    // A foreign commit is pulled between this response's sampled `after` and
    // its own row. The same after <= checkpoint < through rule allows it.
    checkpoint.assignWithinEpoch(101);
    expect(checkpoint.effectiveCoverage()).toBe(102);
    expect(
      await checkpoint.advanceIfContiguous(
        100,
        102,
        resetVersion,
        101,
        async () => {},
      ),
    ).toBe(true);
    checkpoint.settleInFlightCoverage(token);
    expect(checkpoint.value).toBe(102);
    expect(
      entitySequenceInvalidationCovered(
        checkpoint.effectiveCoverage(),
        102,
        false,
      ),
    ).toBe(true);
  });

  it("keeps S14 and S14b at zero pulls when a foreign commit is already held", async () => {
    for (const frameOrder of ["during-response", "after-response"] as const) {
      const checkpoint = new EntitySequenceCheckpoint();
      checkpoint.assign(100);
      const resetVersion = checkpoint.resetVersion;
      const coverage = checkpoint.beginInFlightCoverage(100, 102)!;
      checkpoint.assignWithinEpoch(101);
      let pulls = 0;

      if (frameOrder === "during-response") {
        expect(checkpoint.coversWithInFlight(102)).toBe(true);
        checkpoint.markInFlightCoverageUsed(102);
      }
      expect(
        await checkpoint.advanceIfContiguous(
          100,
          102,
          resetVersion,
          101,
          async () => {},
        ),
      ).toBe(true);
      checkpoint.settleInFlightCoverage(coverage);
      expect(
        entitySequenceInvalidationCovered(
          checkpoint.effectiveCoverage(),
          102,
          false,
        ),
        frameOrder,
      ).toBe(true);
      expect(pulls, frameOrder).toBe(0);
    }
  });

  it("does not register a gap and pulls for its invalidation", async () => {
    const checkpoint = new EntitySequenceCheckpoint();
    checkpoint.assign(100);
    expect(checkpoint.beginInFlightCoverage(101, 110)).toBeUndefined();
    let pulls = 0;
    expect(
      entitySequenceInvalidationCovered(
        checkpoint.effectiveCoverage(),
        105,
        false,
      ),
    ).toBe(false);
    pulls++;
    expect(pulls).toBe(1);
  });

  it.each(["reject", "250ms timeout"])(
    "falls back once after a held persister %s",
    (failure) => {
      const checkpoint = new EntitySequenceCheckpoint();
      checkpoint.assign(100);
      const token = checkpoint.beginInFlightCoverage(100, 110)!;
      const fallback = vi.fn();
      checkpoint.onInFlightCoverageInvalidated(fallback);
      checkpoint.markInFlightCoverageUsed(105);
      checkpoint.settleInFlightCoverage(token);
      expect(fallback, failure).toHaveBeenCalledOnce();
      expect(checkpoint.effectiveCoverage()).toBe(100);
    },
  );

  it("invalidates later contiguous coverage when an earlier response fails", () => {
    const checkpoint = new EntitySequenceCheckpoint();
    checkpoint.assign(100);
    const first = checkpoint.beginInFlightCoverage(100, 110)!;
    const second = checkpoint.beginInFlightCoverage(110, 120)!;
    const fallback = vi.fn();
    checkpoint.onInFlightCoverageInvalidated(fallback);
    checkpoint.markInFlightCoverageUsed(120);

    checkpoint.settleInFlightCoverage(first);
    expect(checkpoint.effectiveCoverage()).toBe(100);
    expect(fallback).toHaveBeenCalledOnce();
    checkpoint.settleInFlightCoverage(second);
    expect(fallback).toHaveBeenCalledOnce();
  });

  it("settles overlapping responses out of order without retaining their tokens", async () => {
    const checkpoint = new EntitySequenceCheckpoint();
    checkpoint.assign(100);
    const first = checkpoint.beginInFlightCoverage(100, 110)!;
    const second = checkpoint.beginInFlightCoverage(110, 120)!;
    expect(checkpoint.effectiveCoverage()).toBe(120);
    checkpoint.settleInFlightCoverage(second);
    expect(checkpoint.effectiveCoverage()).toBe(110);
    checkpoint.settleInFlightCoverage(first);
    expect(checkpoint.effectiveCoverage()).toBe(100);
    expect(
      await checkpoint.advanceIfContiguous(
        110,
        120,
        checkpoint.resetVersion,
        100,
        async () => {},
      ),
    ).toBe(false);
  });

  it("clears in-flight coverage on reset and rejects the stale persister", async () => {
    const checkpoint = new EntitySequenceCheckpoint();
    checkpoint.assign(100);
    const resetVersion = checkpoint.resetVersion;
    const token = checkpoint.beginInFlightCoverage(100, 120)!;
    checkpoint.markInFlightCoverageUsed(120);
    checkpoint.reset();
    checkpoint.settleInFlightCoverage(token);
    expect(checkpoint.effectiveCoverage()).toBe(-1);
    expect(checkpoint.coversWithInFlight(120)).toBe(false);
    expect(
      await checkpoint.advanceIfContiguous(
        100,
        120,
        resetVersion,
        100,
        async () => {},
      ),
    ).toBe(false);
  });

  it("shares one object across re-acquisition and rejects other workspace or schema", async () => {
    const workspace = "checkpoint-workspace";
    const schema = "checkpoint-schema";
    const checkpoint = setEntitySequenceProjection(workspace, schema);
    checkpoint.assign(950);
    const token = checkpoint.beginInFlightCoverage(950, 1021)!;
    let finish!: () => void;
    const persistence = new Promise<void>((resolve) => {
      finish = resolve;
    });
    const persister = persistence.then(() =>
      checkpoint.advanceIfContiguous(
        950,
        1021,
        checkpoint.resetVersion,
        950,
        async () => {},
      ),
    );

    expect(getEntitySequenceCheckpoint("state:entities:v1", schema)).toBe(
      checkpoint,
    );
    expect(
      getEntitySequenceCheckpointForWorkspace(
        "other-workspace",
        "state:entities:v1",
        schema,
      ),
    ).toBeUndefined();
    expect(
      getEntitySequenceCheckpointForWorkspace(
        workspace,
        "state:entities:v1",
        "other-schema",
      ),
    ).toBeUndefined();
    expect(
      getEntitySequenceCheckpointForWorkspace(
        workspace,
        "transcript:one",
        schema,
      ),
    ).toBeUndefined();
    finish();
    expect(await persister).toBe(true);
    checkpoint.settleInFlightCoverage(token);
    expect(checkpoint.value).toBe(1021);
  });

  it("resets the shared object when the active projection changes workspace or schema", () => {
    const first = setEntitySequenceProjection("workspace-one", "schema-one");
    first.assign(120);
    const second = setEntitySequenceProjection("workspace-two", "schema-one");
    expect(second).toBe(first);
    expect(second.value).toBe(-1);
    second.assign(150);
    const schemaChanged = setEntitySequenceProjection(
      "workspace-two",
      "schema-two",
    );
    expect(schemaChanged).toBe(first);
    expect(schemaChanged.value).toBe(-1);
    expect(() =>
      getEntitySequenceCheckpoint("state:entities:v1", "schema-one"),
    ).toThrow();
  });
});

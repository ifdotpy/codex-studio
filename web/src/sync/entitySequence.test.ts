import { describe, expect, it } from "vitest";
import {
  advanceEntitySequenceGuards,
  canAcknowledgeEntitySequenceBatch,
  captureEntitySequenceGuards,
  EntitySequenceCheckpoint,
  entitySequenceInvalidationCovered,
  entitySequenceGuardsCanAdvance,
  getEntitySequenceCheckpoint,
  mutationEntityCheckpointAdvance,
  pullUnlessEntitySequenceInvalidationCovered,
} from "./entitySequence";

describe("entity sequence checkpoint", () => {
  it("keeps a persisted cursor when establishing the first stream epoch", () => {
    const checkpoint = new EntitySequenceCheckpoint();
    checkpoint.assign(1000);

    expect(checkpoint.observeEpoch("current")).toBe(false);
    expect(checkpoint.value).toBe(1000);
  });

  it("shares one checkpoint registry entry between pull and mutation paths", () => {
    const pullCheckpoint = getEntitySequenceCheckpoint(
      "checkpoint-registry-test",
      "state:entities:v1",
      "schema-hash-one",
    );
    const mutationCheckpoint = getEntitySequenceCheckpoint(
      "checkpoint-registry-test",
      "state:entities:v1",
      "schema-hash-one",
    );

    expect(mutationCheckpoint).toBe(pullCheckpoint);
  });

  it("keys entity checkpoints by API schema hash and scope", () => {
    const currentSchemaCheckpoint = getEntitySequenceCheckpoint(
      "schema-keying-test",
      "state:entities:v1",
      "schema-hash-current",
    );
    currentSchemaCheckpoint.assign(1024);

    const otherSchemaCheckpoint = getEntitySequenceCheckpoint(
      "schema-keying-test",
      "state:entities:v1",
      "schema-hash-other",
    );

    expect(otherSchemaCheckpoint).not.toBe(currentSchemaCheckpoint);
    expect(otherSchemaCheckpoint.initialized).toBe(false);
    expect(otherSchemaCheckpoint.value).toBe(-1);
    expect(
      getEntitySequenceCheckpoint(
        "schema-keying-test",
        "state:entities:v1",
        "schema-hash-current",
      ),
    ).toBe(currentSchemaCheckpoint);
  });

  it("does not retain checkpoints for non-entity projection scopes", () => {
    const first = getEntitySequenceCheckpoint(
      "transcript-registry-test",
      "transcript:one",
      "schema-hash-one",
    );
    const second = getEntitySequenceCheckpoint(
      "transcript-registry-test",
      "transcript:one",
      "schema-hash-one",
    );

    expect(second).not.toBe(first);
  });

  it("keeps the registry object through re-acquisition while a persister waits", async () => {
    const pullCheckpoint = getEntitySequenceCheckpoint(
      "reacquire-while-persisting-test",
      "state:entities:v1",
      "schema-hash-one",
    );
    pullCheckpoint.assign(950);
    const guards = captureEntitySequenceGuards(pullCheckpoint);
    let resumePersistence: (() => void) | undefined;
    const persistence = new Promise<void>((resolve) => {
      resumePersistence = resolve;
    });
    const persister = (async () => {
      await persistence;
      return advanceEntitySequenceGuards(guards, 950, 1021);
    })();

    // The projection may close and be acquired again while persistence waits;
    // its new pull closure must still observe the registry's same object.
    const reacquiredCheckpoint = getEntitySequenceCheckpoint(
      "reacquire-while-persisting-test",
      "state:entities:v1",
      "schema-hash-one",
    );
    expect(reacquiredCheckpoint).toBe(pullCheckpoint);
    resumePersistence?.();

    expect(await persister).toBe(true);
    expect(reacquiredCheckpoint.value).toBe(1021);
    const oldWorkspaceGuards = captureEntitySequenceGuards(pullCheckpoint);

    reacquiredCheckpoint.observeEpoch("before-restore");
    reacquiredCheckpoint.assign(1021);
    const beforeEpochChange = getEntitySequenceCheckpoint(
      "reacquire-while-persisting-test",
      "state:entities:v1",
      "schema-hash-one",
    );
    expect(reacquiredCheckpoint.observeEpoch("after-restore")).toBe(true);
    expect(beforeEpochChange).toBe(reacquiredCheckpoint);
    expect(beforeEpochChange.value).toBe(-1);
    expect(beforeEpochChange.canUseDurableCheckpoint).toBe(false);
    beforeEpochChange.assign(17);
    beforeEpochChange.reset();
    expect(
      getEntitySequenceCheckpoint(
        "reacquire-while-persisting-test",
        "state:entities:v1",
        "schema-hash-one",
      ),
    ).toBe(beforeEpochChange);
    expect(beforeEpochChange.value).toBe(-1);

    const workspaceChangedCheckpoint = getEntitySequenceCheckpoint(
      "different-workspace-test",
      "state:entities:v1",
      "schema-hash-one",
    );
    expect(workspaceChangedCheckpoint).toBe(pullCheckpoint);
    expect(pullCheckpoint.value).toBe(-1);
    expect(advanceEntitySequenceGuards(oldWorkspaceGuards, 1021, 1024)).toBe(
      false,
    );
  });

  it("replaces a high checkpoint with a lower checkpoint after an epoch change", () => {
    const checkpoint = new EntitySequenceCheckpoint();
    checkpoint.observeEpoch("before-restore");
    checkpoint.assign(1000);

    expect(checkpoint.observeEpoch("after-restore")).toBe(true);
    expect(checkpoint.covers(901)).toBe(false);
    checkpoint.assign(900);
    expect(checkpoint.covers(901)).toBe(false);
    expect(checkpoint.covers(900)).toBe(true);
  });

  it("clears its skip sentinel when the server requests a reset", () => {
    const checkpoint = new EntitySequenceCheckpoint();
    checkpoint.observeEpoch("epoch");
    checkpoint.assign(1000);
    checkpoint.reset();
    expect(checkpoint.covers(1)).toBe(false);
    checkpoint.assign(12);
    expect(checkpoint.value).toBe(12);
    expect(checkpoint.covers(40)).toBe(false);
  });

  it("marks a pull stale only when an epoch change or reset occurs", () => {
    const checkpoint = new EntitySequenceCheckpoint();
    checkpoint.observeEpoch("epoch");
    checkpoint.assign(40);
    const pullResetVersion = checkpoint.resetVersion;

    expect(checkpoint.isSameResetVersion(pullResetVersion)).toBe(true);
    checkpoint.reset();
    expect(checkpoint.isSameResetVersion(pullResetVersion)).toBe(false);
  });

  it("advances a mutation only when its before checkpoint still matches", () => {
    const checkpoint = new EntitySequenceCheckpoint();
    checkpoint.assign(71_301);
    const guards = captureEntitySequenceGuards(checkpoint);

    expect(advanceEntitySequenceGuards(guards, 71_301, 71_305)).toBe(true);
    expect(checkpoint.value).toBe(71_305);
    expect(advanceEntitySequenceGuards(guards, 71_301, 71_307)).toBe(false);
    expect(advanceEntitySequenceGuards(guards, 71_303, 71_309)).toBe(false);
    expect(advanceEntitySequenceGuards(guards, 71_299, 71_309)).toBe(false);
    expect(checkpoint.value).toBe(71_305);
  });

  it("advances only for a present watermark equal to the local checkpoint", () => {
    const advance = (checkpoint: number, after: number | undefined) =>
      mutationEntityCheckpointAdvance(checkpoint, after, 100, true, true);

    expect(advance(90, 90)).toBe(100);
    expect(advance(89, 90)).toBeUndefined();
    expect(advance(91, 90)).toBeUndefined();
    expect(advance(90, undefined)).toBeUndefined();
  });

  it("adopts a durable checkpoint from the uninitialized sentinel when reset generation is unchanged", () => {
    const checkpoint = new EntitySequenceCheckpoint();
    const guards = captureEntitySequenceGuards(checkpoint);
    const advanceTo = mutationEntityCheckpointAdvance(
      950,
      950,
      1024,
      true,
      entitySequenceGuardsCanAdvance(guards, 950),
    );

    expect(advanceTo).toBe(1024);
    expect(advanceEntitySequenceGuards(guards, 950, advanceTo!)).toBe(true);
    expect(checkpoint.value).toBe(1024);
  });

  it("does not adopt a persisted checkpoint after a reset invalidates the sentinel guard", () => {
    const checkpoint = new EntitySequenceCheckpoint();
    const guards = captureEntitySequenceGuards(checkpoint);
    checkpoint.reset();

    expect(entitySequenceGuardsCanAdvance(guards, 950)).toBe(false);
    expect(advanceEntitySequenceGuards(guards, 950, 1024)).toBe(false);
    expect(checkpoint.value).toBe(-1);
    expect(checkpoint.canUseDurableCheckpoint).toBe(false);
    const afterResetGuards = captureEntitySequenceGuards(checkpoint);
    expect(entitySequenceGuardsCanAdvance(afterResetGuards, 950)).toBe(false);
  });

  it("does not let out-of-order mutations or an older pull lower the checkpoint", () => {
    const checkpoint = new EntitySequenceCheckpoint();
    checkpoint.assign(71_311);
    const earlierMutation = captureEntitySequenceGuards(checkpoint);
    const laterMutation = captureEntitySequenceGuards(checkpoint);

    expect(advanceEntitySequenceGuards(laterMutation, 71_311, 71_319)).toBe(
      true,
    );
    expect(advanceEntitySequenceGuards(earlierMutation, 71_311, 71_315)).toBe(
      false,
    );
    checkpoint.assignWithinEpoch(71_315);
    expect(checkpoint.value).toBe(71_319);
  });

  it("rejects a mutation acknowledgement after an epoch or reset change", () => {
    const checkpoint = new EntitySequenceCheckpoint();
    checkpoint.observeEpoch("before");
    checkpoint.assign(71_321);
    const guards = captureEntitySequenceGuards(checkpoint);

    checkpoint.observeEpoch("after");
    expect(advanceEntitySequenceGuards(guards, 71_321, 71_323)).toBe(false);
    checkpoint.assign(71_321);
    const resetGuards = captureEntitySequenceGuards(checkpoint);
    checkpoint.reset();
    expect(advanceEntitySequenceGuards(resetGuards, 71_321, 71_325)).toBe(
      false,
    );
  });

  it("pulls once when frames arrive before or during mutation persistence", async () => {
    for (const frameTiming of ["before", "during"] as const) {
      let acknowledged = false;
      let pulls = 0;
      let checkpoint = 89;
      let persistenceInFlight = false;
      const frame = async () => {
        if (acknowledged) return undefined;
        const arrival = persistenceInFlight ? "during" : "before";
        const outcome = await pullUnlessEntitySequenceInvalidationCovered(
          checkpoint,
          90,
          false,
          async () => {
            pulls++;
          },
        );
        if (outcome.skipped) acknowledged = true;
        return arrival;
      };
      if (frameTiming === "before") {
        expect(await frame()).toBe("before");
        persistenceInFlight = true;
      } else {
        persistenceInFlight = true;
        expect(await frame()).toBe("during");
      }
      checkpoint = 90;
      acknowledged = canAcknowledgeEntitySequenceBatch(
        checkpoint,
        90,
        true,
        true,
      );
      persistenceInFlight = false;
      await frame();
      expect(pulls, frameTiming).toBe(1);
    }
  });

  it("suppresses a frame only after successful persistence covers its rows", () => {
    let acknowledged = false;
    let pulls = 0;
    const frame = () => {
      if (!acknowledged) pulls++;
    };

    acknowledged = canAcknowledgeEntitySequenceBatch(91, 90, true, true);
    frame();

    expect(acknowledged).toBe(true);
    expect(pulls).toBe(0);
  });

  it("skips a pending versioned pull when persistence covers it before dispatch", async () => {
    const requiredSequence = 1021;
    const checkpoint = getEntitySequenceCheckpoint(
      "dispatch-race-test",
      "state:entities:v1",
      "schema-hash-one",
    );
    checkpoint.assign(950);
    const mutationCheckpoint = getEntitySequenceCheckpoint(
      "dispatch-race-test",
      "state:entities:v1",
      "schema-hash-one",
    );
    const guards = captureEntitySequenceGuards(mutationCheckpoint);
    let requests = 0;

    expect(
      entitySequenceInvalidationCovered(
        checkpoint.value,
        requiredSequence,
        false,
      ),
    ).toBe(false);
    // Mutation persistence completes after invalidation delivery but before
    // the refresh reaches its pull dispatch point.
    expect(advanceEntitySequenceGuards(guards, 950, requiredSequence)).toBe(
      true,
    );
    const outcome = await pullUnlessEntitySequenceInvalidationCovered(
      checkpoint.value,
      requiredSequence,
      false,
      async () => {
        requests++;
        return "pulled";
      },
    );

    expect(outcome).toEqual({ skipped: true });
    expect(requests).toBe(0);
  });

  it("still issues a pull for a reset invalidation when the numeric checkpoint is high", async () => {
    let requests = 0;
    const outcome = await pullUnlessEntitySequenceInvalidationCovered(
      1021,
      undefined,
      true,
      async () => {
        requests++;
        return "pulled";
      },
    );

    expect(outcome).toEqual({ skipped: false, value: "pulled" });
    expect(requests).toBe(1);
  });

  it.each(["persistence failure", "250 ms timeout"])(
    "pulls a frame after %s instead of suppressing it",
    () => {
      const acknowledged = canAcknowledgeEntitySequenceBatch(
        90,
        90,
        false,
        true,
      );
      let pulls = 0;
      if (!acknowledged) pulls++;
      expect(pulls).toBe(1);
    },
  );

  it.each([
    ["failed persistence", 90, 90, false, true],
    ["persistence timed out at 250 ms", 90, 90, false, true],
    ["checkpoint is lower", 89, 90, true, true],
    [
      "checkpoint is between the watermark and response row",
      95,
      100,
      true,
      true,
    ],
  ])(
    "does not acknowledge after %s",
    (_case, checkpoint, highest, current, guardsMatch) => {
      expect(
        canAcknowledgeEntitySequenceBatch(
          checkpoint,
          highest,
          current,
          guardsMatch,
        ),
      ).toBe(false);
    },
  );

  it("does not acknowledge two mutations that resolve out of order", () => {
    const checkpoint = new EntitySequenceCheckpoint();
    checkpoint.assign(120);
    const first = captureEntitySequenceGuards(checkpoint);
    const second = captureEntitySequenceGuards(checkpoint);

    expect(advanceEntitySequenceGuards(second, 120, 130)).toBe(true);
    expect(
      canAcknowledgeEntitySequenceBatch(
        checkpoint.value,
        125,
        true,
        first[0]?.checkpoint.value === 120,
      ),
    ).toBe(false);
    expect(checkpoint.value).toBe(130);
  });
});

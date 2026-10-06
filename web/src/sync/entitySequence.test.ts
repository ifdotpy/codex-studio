import { describe, expect, it } from "vitest";
import { EntitySequenceCheckpoint } from "./entitySequence";

describe("entity sequence checkpoint", () => {
  it("keeps a persisted cursor when establishing the first stream epoch", () => {
    const checkpoint = new EntitySequenceCheckpoint();
    checkpoint.assign(1000);

    expect(checkpoint.observeEpoch("current")).toBe(false);
    expect(checkpoint.value).toBe(1000);
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
});

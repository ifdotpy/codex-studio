import { expect, it, vi } from "vitest";
import { createNativeResourceReads } from "./resourceReads";
import type { ResourceVersion } from "../../sync/resourceEvents";

it("does not reread an in-flight native baseline and retains a later revision", async () => {
  let changed!: (version?: ResourceVersion) => void;
  let release!: () => void;
  let delivered!: () => void;
  const blocked = new Promise<void>((resolve) => {
    release = resolve;
  });
  const second = new Promise<void>((resolve) => {
    delivered = resolve;
  });
  const read = vi.fn(async (_version?: ResourceVersion) => {
    if (read.mock.calls.length === 1) await blocked;
    else delivered();
  });
  const watch = createNativeResourceReads({
    watchResourceChanges: (_ref, callback) => {
      changed = callback;
      return () => {};
    },
    watchResourceConnection: () => () => {},
  });
  const stop = watch({ kind: "accounts" }, read, vi.fn(), {
    waitForBaseline: true,
  });
  const first = { epoch: "epoch", revision: 1 };
  changed(first);
  await Promise.resolve();
  changed(first);
  release();
  await Promise.resolve();
  await Promise.resolve();
  expect(read).toHaveBeenCalledTimes(1);
  changed({ epoch: "epoch", revision: 2 });
  await second;
  expect(read).toHaveBeenCalledTimes(2);
  expect(read.mock.calls.map(([version]) => version?.revision)).toEqual([1, 2]);
  stop();
});

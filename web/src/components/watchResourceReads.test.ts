import { beforeEach, describe, expect, it, vi } from "vitest";
import type { components } from "../generated/api";

type ResourceRef = components["schemas"]["ResourceRef"];

const { watchers } = vi.hoisted(() => ({
  watchers: new Map<string, () => void>(),
}));

vi.mock("../sync/resourceEvents", () => ({
  watchResourceChanges: (
    resource: ResourceRef,
    callback: () => void,
  ): (() => void) => {
    const key = JSON.stringify(resource);
    watchers.set(key, callback);
    return () => watchers.delete(key);
  },
}));

import { watchResourceReads } from "./watchResourceReads";

const terminals: ResourceRef = { kind: "terminals" };
const output: ResourceRef = { kind: "terminal", terminalId: "terminal-1" };
const notify = (resource: ResourceRef) => {
  const callback = watchers.get(JSON.stringify(resource));
  if (!callback) throw new Error("Resource watcher is not registered");
  callback();
};

describe("watchResourceReads", () => {
  beforeEach(() => watchers.clear());

  it("reads on the initial event and stops reading after unsubscribe", async () => {
    const read = vi.fn(async () => {});
    const failed = vi.fn();
    const stop = watchResourceReads(terminals, read, failed);

    const callback = watchers.get(JSON.stringify(terminals));
    if (!callback) throw new Error("Resource watcher is not registered");
    notify(terminals);
    await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(1));
    stop();
    callback();
    await Promise.resolve();
    expect(read).toHaveBeenCalledTimes(1);
    expect(failed).not.toHaveBeenCalled();
  });

  it("reconciles once per change or reconnect notification", async () => {
    const read = vi.fn(async () => {});
    const stop = watchResourceReads(terminals, read, vi.fn());

    notify(terminals); // initial baseline
    await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(1));
    notify(terminals); // resource change
    await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(2));
    notify(terminals); // reconnect baseline
    await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(3));
    stop();
  });

  it("drains a change that arrives during an in-flight read", async () => {
    let finish!: () => void;
    const read = vi
      .fn<() => Promise<void>>()
      .mockImplementationOnce(
        () =>
          new Promise<void>((resolve) => {
            finish = resolve;
          }),
      )
      .mockResolvedValue(undefined);
    const stop = watchResourceReads(output, read, vi.fn());

    notify(output);
    await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(1));
    notify(output);
    expect(read).toHaveBeenCalledTimes(1);

    finish();
    await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(2));
    stop();
  });

  it("reports a failed read and retries only after another resource event", async () => {
    const read = vi
      .fn<() => Promise<void>>()
      .mockRejectedValueOnce(new Error("offline"))
      .mockResolvedValue(undefined);
    const failed = vi.fn();
    const stop = watchResourceReads(terminals, read, failed);

    notify(terminals);
    await vi.waitFor(() =>
      expect(failed).toHaveBeenCalledWith(expect.any(Error)),
    );
    expect(read).toHaveBeenCalledTimes(1);

    notify(terminals);
    await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(2));
    stop();
  });

  it("coalesces initial notifications for a set of resources", async () => {
    const read = vi.fn(async () => {});
    const stop = watchResourceReads([terminals, output], read, vi.fn());

    notify(terminals);
    notify(output);
    await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(1));
    stop();
  });
});

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ResourceRef, ResourceVersion } from "../sync/resourceEvents";

const {
  watchers,
  resumeListeners,
  connectionListeners,
  connectionState,
  knownVersions,
} = vi.hoisted(() => ({
  watchers: new Map<string, (version?: ResourceVersion) => void>(),
  resumeListeners: new Set<() => void>(),
  connectionListeners: new Set<
    (status: string, stream: "stopped" | "connecting" | "open") => void
  >(),
  connectionState: {
    status: "connecting" as string,
    stream: "connecting" as "stopped" | "connecting" | "open",
  },
  knownVersions: new Map<string, ResourceVersion>(),
}));

vi.mock("../sync/resourceEvents", () => ({
  watchResourceChanges: (
    resource: ResourceRef,
    callback: (version?: ResourceVersion) => void,
    _options?: { waitForBaseline?: boolean },
  ): (() => void) => {
    const key = JSON.stringify(resource);
    watchers.set(key, callback);
    const known = knownVersions.get(key);
    if (known) callback(known);
    return () => watchers.delete(key);
  },
  watchResourceConnection: (
    callback: (
      status: string,
      stream: "stopped" | "connecting" | "open",
    ) => void,
  ) => {
    connectionListeners.add(callback);
    callback(connectionState.status, connectionState.stream);
    return () => connectionListeners.delete(callback);
  },
}));
vi.mock("../sync/resume", () => ({
  onResume: (callback: () => void) => {
    resumeListeners.add(callback);
    return () => resumeListeners.delete(callback);
  },
}));

import { watchResourceReads } from "./watchResourceReads";

const terminals: ResourceRef = { kind: "terminals" };
const limits: ResourceRef = { kind: "limits", accountKey: "default" };
const output: ResourceRef = { kind: "terminal", terminalId: "terminal-1" };
const notify = (resource: ResourceRef, version?: ResourceVersion) => {
  if (version) knownVersions.set(JSON.stringify(resource), version);
  const callback = watchers.get(JSON.stringify(resource));
  if (!callback) throw new Error("Resource watcher is not registered");
  callback(version);
};

describe("watchResourceReads", () => {
  beforeEach(() => {
    watchers.clear();
    resumeListeners.clear();
    connectionListeners.clear();
    knownVersions.clear();
    connectionState.status = "connecting";
    connectionState.stream = "connecting";
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("refreshes account limits after their typed resource event", async () => {
    vi.useFakeTimers();
    const displayed: number[] = [];
    const read = vi.fn(async () => {
      displayed.push(displayed.length === 0 ? 20 : 80);
    });
    const stop = watchResourceReads(limits, read, vi.fn());
    notify(limits);
    await vi.advanceTimersByTimeAsync(0);
    expect(displayed).toEqual([20]);
    notify(limits);
    await vi.advanceTimersByTimeAsync(0);
    expect(displayed).toEqual([20, 80]);
    expect(read).toHaveBeenCalledTimes(2);
    stop();
  });

  it("passes the newest resource version through a coalesced read", async () => {
    let finish!: () => void;
    const read = vi
      .fn<(version?: ResourceVersion) => Promise<void>>()
      .mockImplementationOnce(
        () =>
          new Promise<void>((resolve) => {
            finish = resolve;
          }),
      )
      .mockResolvedValue(undefined);
    const stop = watchResourceReads(limits, read, vi.fn());

    notify(limits, { epoch: "workspace", revision: 1 });
    await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(1));
    notify(limits, { epoch: "workspace", revision: 2 });
    finish();

    await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(2));
    expect(read.mock.calls[0]?.[0]).toEqual({
      epoch: "workspace",
      revision: 1,
    });
    expect(read.mock.calls[1]?.[0]).toEqual({
      epoch: "workspace",
      revision: 2,
    });
    stop();
  });

  it("reads once for the first baseline and skips a repeated unchanged baseline", async () => {
    const read = vi.fn(async () => {});
    const stop = watchResourceReads(limits, read, vi.fn(), {
      waitForBaseline: true,
    });

    notify(limits, { epoch: "workspace", revision: 4 });
    await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(1));
    notify(limits, { epoch: "workspace", revision: 4 });
    await Promise.resolve();
    expect(read).toHaveBeenCalledTimes(1);
    stop();
  });

  it("reads again when a baseline has a new revision or epoch", async () => {
    const read = vi.fn(async () => {});
    const stop = watchResourceReads(limits, read, vi.fn(), {
      waitForBaseline: true,
    });

    notify(limits, { epoch: "workspace", revision: 4 });
    await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(1));
    notify(limits, { epoch: "workspace", revision: 5 });
    await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(2));
    notify(limits, { epoch: "new-workspace", revision: 5 });
    await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(3));
    stop();
  });

  it("retries a failed baseline when the same baseline is delivered again", async () => {
    const read = vi
      .fn<() => Promise<void>>()
      .mockRejectedValueOnce(new Error("permanent read failure"))
      .mockResolvedValue(undefined);
    const failed = vi.fn();
    const stop = watchResourceReads(limits, read, failed, {
      waitForBaseline: true,
    });

    notify(limits, { epoch: "workspace", revision: 4 });
    await vi.waitFor(() => expect(failed).toHaveBeenCalledTimes(1));
    expect(read).toHaveBeenCalledTimes(1);
    notify(limits, { epoch: "workspace", revision: 4 });
    await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(2));
    stop();
  });

  it("replays a known baseline to a watcher registered after it", async () => {
    const version = { epoch: "workspace", revision: 4 };
    knownVersions.set(JSON.stringify(limits), version);
    const read = vi.fn(async () => {});
    const stop = watchResourceReads(limits, read, vi.fn(), {
      waitForBaseline: true,
    });

    await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(1));
    expect(read).toHaveBeenCalledWith(version);
    stop();
  });

  it("uses one fallback read when no stream is available", async () => {
    vi.useFakeTimers();
    connectionState.status = "offline";
    connectionState.stream = "stopped";
    const read = vi.fn(async () => {});
    const stop = watchResourceReads(limits, read, vi.fn(), {
      waitForBaseline: true,
    });

    await vi.advanceTimersByTimeAsync(0);
    expect(read).toHaveBeenCalledTimes(1);
    stop();
  });

  it("recovers a failed read without another resource change", async () => {
    vi.useFakeTimers();
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    const read = vi
      .fn<() => Promise<void>>()
      .mockRejectedValueOnce(new TypeError("Failed to fetch"))
      .mockResolvedValue(undefined);
    const failed = vi.fn();
    const stop = watchResourceReads(terminals, read, failed);
    notify(terminals);
    await vi.advanceTimersByTimeAsync(0);
    expect(read).toHaveBeenCalledTimes(1);
    expect(failed).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(999);
    expect(read).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(read).toHaveBeenCalledTimes(2);
    await vi.advanceTimersByTimeAsync(60_000);
    expect(read).toHaveBeenCalledTimes(2);
    stop();
  });

  it.each(["offline", "hidden"])(
    "pauses a pending retry while %s and reads once on resume",
    async (reason) => {
      vi.useFakeTimers();
      const windowListeners = new Map<string, () => void>();
      const documentListeners = new Map<string, () => void>();
      const documentStub = {
        hidden: false,
        addEventListener: (name: string, listener: () => void) =>
          documentListeners.set(name, listener),
        removeEventListener: (name: string) => documentListeners.delete(name),
      };
      const navigatorStub = { onLine: true };
      vi.stubGlobal("document", documentStub);
      vi.stubGlobal("navigator", navigatorStub);
      vi.stubGlobal("window", {
        addEventListener: (name: string, listener: () => void) =>
          windowListeners.set(name, listener),
        removeEventListener: (name: string) => windowListeners.delete(name),
      });
      const read = vi
        .fn<() => Promise<void>>()
        .mockRejectedValueOnce(new TypeError("Failed to fetch"))
        .mockResolvedValue(undefined);
      const stop = watchResourceReads(output, read, vi.fn());
      notify(output);
      await vi.advanceTimersByTimeAsync(0);
      expect(read).toHaveBeenCalledTimes(1);
      if (reason === "offline") {
        navigatorStub.onLine = false;
        windowListeners.get("offline")?.();
      } else {
        documentStub.hidden = true;
        documentListeners.get("visibilitychange")?.();
      }
      notify(output);
      await vi.advanceTimersByTimeAsync(60_000);
      expect(read).toHaveBeenCalledTimes(1);
      expect(vi.getTimerCount()).toBe(0);
      navigatorStub.onLine = true;
      documentStub.hidden = false;
      for (const resume of resumeListeners) resume();
      await vi.advanceTimersByTimeAsync(0);
      expect(read).toHaveBeenCalledTimes(2);
      await vi.advanceTimersByTimeAsync(60_000);
      expect(read).toHaveBeenCalledTimes(2);
      stop();
      expect(windowListeners.size).toBe(0);
      expect(documentListeners.size).toBe(0);
    },
  );

  it("cancels retries after disposal and ignores an in-flight failure", async () => {
    vi.useFakeTimers();
    let reject!: (error: unknown) => void;
    const read = vi.fn(
      () =>
        new Promise<void>((_, fail) => {
          reject = fail;
        }),
    );
    const failed = vi.fn();
    const stop = watchResourceReads(terminals, read, failed);
    notify(terminals);
    await vi.advanceTimersByTimeAsync(0);
    stop();
    reject(new TypeError("Failed to fetch"));
    await vi.advanceTimersByTimeAsync(60_000);
    expect(read).toHaveBeenCalledTimes(1);
    expect(failed).not.toHaveBeenCalled();
    expect(vi.getTimerCount()).toBe(0);

    const pendingRead = vi.fn(async () => {
      throw new TypeError("Failed to fetch");
    });
    const stopPending = watchResourceReads(terminals, pendingRead, vi.fn());
    notify(terminals);
    await vi.advanceTimersByTimeAsync(0);
    expect(vi.getTimerCount()).toBe(1);
    stopPending();
    await vi.advanceTimersByTimeAsync(60_000);
    expect(pendingRead).toHaveBeenCalledTimes(1);
    expect(vi.getTimerCount()).toBe(0);
  });

  it("keeps repeated failures serial and increases the retry delay", async () => {
    vi.useFakeTimers();
    const read = vi.fn(async () => {
      throw new TypeError("Failed to fetch");
    });
    const stop = watchResourceReads(terminals, read, vi.fn());
    notify(terminals);
    await vi.advanceTimersByTimeAsync(0);
    await vi.advanceTimersByTimeAsync(60_000);
    expect(read).toHaveBeenCalledTimes(6);
    expect(vi.getTimerCount()).toBe(1);
    stop();
  });

  it("preserves the backoff during frequent resource changes", async () => {
    vi.useFakeTimers();
    const read = vi.fn(async () => {
      throw new TypeError("Failed to fetch");
    });
    const stop = watchResourceReads(output, read, vi.fn());
    notify(output);
    await vi.advanceTimersByTimeAsync(0);
    for (let index = 0; index < 20; index++) {
      notify(output);
      await vi.advanceTimersByTimeAsync(100);
    }
    expect(read).toHaveBeenCalledTimes(2);
    expect(vi.getTimerCount()).toBe(1);
    stop();
  });

  it("recovers the original read when its failure reporter throws", async () => {
    vi.useFakeTimers();
    const read = vi
      .fn<() => Promise<void>>()
      .mockRejectedValueOnce(new TypeError("Failed to fetch"))
      .mockResolvedValue(undefined);
    const failed = vi.fn(() => {
      throw new Error("Reporter failed");
    });
    const stop = watchResourceReads(output, read, failed);
    notify(output);
    await vi.advanceTimersByTimeAsync(0);
    expect(failed).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(1_000);
    expect(read).toHaveBeenCalledTimes(2);
    stop.refresh();
    await vi.advanceTimersByTimeAsync(0);
    expect(read).toHaveBeenCalledTimes(3);
    stop();
    stop.refresh();
    await vi.advanceTimersByTimeAsync(60_000);
    expect(read).toHaveBeenCalledTimes(3);
  });

  it("does not install a retry after the failure reporter disposes the watcher", async () => {
    vi.useFakeTimers();
    const read = vi.fn(async () => {
      throw new TypeError("Failed to fetch");
    });
    const stop = watchResourceReads(output, read, () => stop());
    notify(output);
    await vi.advanceTimersByTimeAsync(60_000);
    expect(read).toHaveBeenCalledTimes(1);
    expect(vi.getTimerCount()).toBe(0);
  });

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

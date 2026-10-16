import { afterEach, describe, expect, it, vi } from "vitest";
import { ApiError } from "../api";
import { uploadsChanged } from "../sync/uploads";
import {
  attemptUploadDelivery,
  createUploadRecoveryRetryScheduler,
  retryableUploadFailure,
  retryableUploadStoreFailure,
  subscribeUploadRecoveryWakeups,
} from "./useUploadRecovery";
import type { PendingUpload } from "../sync/uploads";

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("upload recovery scheduling", () => {
  it("clears the retry timer when no failed work remains", async () => {
    vi.useFakeTimers();
    const run = vi.fn();
    const scheduler = createUploadRecoveryRetryScheduler(run);

    scheduler.schedule(["upload-1"]);
    scheduler.schedule([]);
    await vi.advanceTimersByTimeAsync(120_000);

    expect(vi.getTimerCount()).toBe(0);
    expect(run).not.toHaveBeenCalled();
  });

  it("bounds retries for a failed pending-upload read without known IDs", async () => {
    vi.useFakeTimers();
    const run = vi.fn();
    const scheduler = createUploadRecoveryRetryScheduler(run);

    scheduler.schedule(null);
    await vi.advanceTimersByTimeAsync(1_000);

    expect(run).toHaveBeenCalledTimes(1);
    expect(run).toHaveBeenCalledWith(null);
  });

  it("caps selected-upload backoff and clears after no failed work remains", async () => {
    vi.useFakeTimers();
    const run = vi.fn((ids: Set<string> | null) => {
      expect(ids).toEqual(new Set(["upload-1"]));
      if (run.mock.calls.length < 8) scheduler.schedule(["upload-1"]);
      else scheduler.schedule([]);
    });
    const scheduler = createUploadRecoveryRetryScheduler(run);

    scheduler.schedule(["upload-1"]);
    await vi.advanceTimersByTimeAsync(119_999);

    expect(run).toHaveBeenCalledTimes(7);
    expect(vi.getTimerCount()).toBe(1);
    await vi.advanceTimersByTimeAsync(1);

    expect(run).toHaveBeenCalledTimes(7);
    await vi.advanceTimersByTimeAsync(1_000);

    expect(run).toHaveBeenCalledTimes(8);
    expect(vi.getTimerCount()).toBe(0);
  });

  it("keeps retrying the same pending upload after a long transient outage", async () => {
    vi.useFakeTimers();
    let scheduler: ReturnType<typeof createUploadRecoveryRetryScheduler>;
    const run = vi.fn((ids: Set<string> | null) => {
      expect(ids).toEqual(new Set(["upload-1"]));
      if (run.mock.calls.length < 9) scheduler.schedule(["upload-1"]);
      else scheduler.schedule([]);
    });
    scheduler = createUploadRecoveryRetryScheduler(run);
    scheduler.schedule(["upload-1"]);

    await vi.advanceTimersByTimeAsync(180_000);

    expect(run).toHaveBeenCalledTimes(9);
    expect(vi.getTimerCount()).toBe(0);
  });

  it("retries transient upload failures but leaves permanent API errors for an explicit retry", () => {
    expect(retryableUploadFailure(new TypeError("network unavailable"))).toBe(
      true,
    );
    expect(retryableUploadFailure(new ApiError("Unavailable", 503))).toBe(true);
    expect(retryableUploadFailure(new ApiError("Unsupported", 501))).toBe(
      false,
    );
    expect(retryableUploadFailure(new ApiError("Invalid file", 400))).toBe(
      false,
    );
  });

  it("automatically retries a row when its IndexedDB delivery transaction aborts", async () => {
    const row = {
      id: "upload-1",
      workspace: "workspace-1",
      stateDir: "/state",
      agent: "lead",
      name: "notes.txt",
      size: 12,
      mime: "text/plain",
      created: 1,
    } as PendingUpload;
    const aborted = new DOMException("Transaction aborted", "AbortError");
    const attempt = await attemptUploadDelivery(row, async (received) => {
      expect(received).toBe(row);
      throw aborted;
    });

    expect(attempt).toEqual({
      ok: false,
      error: aborted,
      retryable: true,
    });
    expect(
      retryableUploadStoreFailure(
        new DOMException("Timed out", "TimeoutError"),
      ),
    ).toBe(true);
    expect(
      retryableUploadStoreFailure(
        new DOMException("Closed", "InvalidStateError"),
      ),
    ).toBe(true);
    expect(
      retryableUploadStoreFailure(new DOMException("Bad row", "DataError")),
    ).toBe(false);
  });

  it("notifies another tab about changes and releases the channel on unsubscribe", async () => {
    class TestBroadcastChannel {
      static channels = new Set<TestBroadcastChannel>();
      readonly listeners = new Set<(event: MessageEvent) => void>();
      closed = false;

      constructor(readonly name: string) {
        TestBroadcastChannel.channels.add(this);
      }

      addEventListener(
        type: string,
        listener: EventListenerOrEventListenerObject,
      ) {
        if (type === "message")
          this.listeners.add(listener as (event: MessageEvent) => void);
      }

      removeEventListener(
        type: string,
        listener: EventListenerOrEventListenerObject,
      ) {
        if (type === "message")
          this.listeners.delete(listener as (event: MessageEvent) => void);
      }

      postMessage(data: unknown) {
        for (const channel of TestBroadcastChannel.channels) {
          if (channel === this || channel.name !== this.name || channel.closed)
            continue;
          queueMicrotask(() => {
            const event = new MessageEvent("message", { data });
            for (const listener of channel.listeners) listener(event);
          });
        }
      }

      close() {
        this.closed = true;
        TestBroadcastChannel.channels.delete(this);
      }
    }

    vi.useFakeTimers();
    vi.stubGlobal("window", new EventTarget());
    vi.stubGlobal(
      "document",
      Object.assign(new EventTarget(), { hidden: false }),
    );
    vi.stubGlobal("BroadcastChannel", TestBroadcastChannel);
    const localWake = vi.fn();
    const remoteWake = vi.fn();
    const unsubscribe = subscribeUploadRecoveryWakeups(localWake);
    const remote = new TestBroadcastChannel("studio-uploads-changed");
    remote.addEventListener("message", remoteWake);
    expect(vi.getTimerCount()).toBe(0);

    uploadsChanged();
    await new Promise<void>((resolve) => queueMicrotask(resolve));

    expect(localWake).toHaveBeenCalledTimes(1);
    expect(remoteWake).toHaveBeenCalledTimes(1);
    expect(vi.getTimerCount()).toBe(0);
    unsubscribe();
    expect(TestBroadcastChannel.channels.size).toBe(1);

    remote.postMessage({ type: "changed" });
    window.dispatchEvent(new Event("studio-uploads-changed"));
    await new Promise<void>((resolve) => queueMicrotask(resolve));
    expect(localWake).toHaveBeenCalledTimes(1);
    expect(remoteWake).toHaveBeenCalledTimes(1);
    remote.close();
  });
});

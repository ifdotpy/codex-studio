import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  effects: [] as Array<() => void | (() => void)>,
  uploadListeners: new Set<() => void>(),
  resumeListeners: new Set<() => void>(),
  pendingUploads: vi.fn(),
  deliverUpload: vi.fn(),
  finishUpload: vi.fn(),
  removeUpload: vi.fn(),
  uploadCancelled: vi.fn(),
}));

vi.mock("react", () => ({
  useEffect: (effect: () => void | (() => void)) => {
    mocks.effects.push(effect);
  },
  useRef: (initial: unknown) => ({ current: initial }),
  useState: (initial: unknown) => [initial, vi.fn()],
}));
vi.mock("../sync/uploads", () => ({
  pendingUploads: mocks.pendingUploads,
  deliverUpload: mocks.deliverUpload,
  finishUpload: mocks.finishUpload,
  removeUpload: mocks.removeUpload,
  uploadCancelled: mocks.uploadCancelled,
  onUploadsChanged: (listener: () => void) => {
    mocks.uploadListeners.add(listener);
    return () => mocks.uploadListeners.delete(listener);
  },
}));
vi.mock("../sync/resume", () => ({
  onResume: (listener: () => void) => {
    mocks.resumeListeners.add(listener);
    return () => mocks.resumeListeners.delete(listener);
  },
}));
vi.mock("../api", () => ({
  errorText: (error: unknown) =>
    error instanceof Error ? error.message : String(error),
}));
vi.mock("../sync/readRetry", () => ({
  retryableReadError: (error: unknown) => error instanceof TypeError,
}));

import { useUploadRecovery } from "./useUploadRecovery";
import type { PendingUpload } from "../sync/uploads";

function mount() {
  useUploadRecovery("/workspace/state", "workspace-1", async () => true);
  const cleanups = mocks.effects.splice(0).flatMap((effect) => {
    const cleanup = effect();
    return cleanup ? [cleanup] : [];
  });
  return () => cleanups.forEach((cleanup) => cleanup());
}

function upload(): PendingUpload {
  return {
    id: "upload-stable-id",
    workspace: "workspace-1",
    stateDir: "/workspace/state",
    agent: "lead",
    name: "notes.txt",
    size: 12,
    mime: "text/plain",
    created: 1,
  };
}

async function flushMicrotasks() {
  for (let attempt = 0; attempt < 30; attempt++) await Promise.resolve();
}

beforeEach(() => {
  vi.useFakeTimers();
  mocks.effects.length = 0;
  mocks.uploadListeners.clear();
  mocks.resumeListeners.clear();
  mocks.pendingUploads.mockReset();
  mocks.deliverUpload.mockReset();
  mocks.finishUpload.mockReset().mockResolvedValue(undefined);
  mocks.removeUpload.mockReset();
  mocks.uploadCancelled.mockReset().mockReturnValue(false);
  vi.stubGlobal("navigator", { onLine: true });
  vi.stubGlobal("window", new EventTarget());
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("useUploadRecovery", () => {
  it("does not poll an empty queue and stops reads after unmount", async () => {
    mocks.pendingUploads.mockResolvedValue([]);
    const unmount = mount();
    await flushMicrotasks();
    expect(mocks.pendingUploads).toHaveBeenCalledTimes(1);

    await vi.advanceTimersByTimeAsync(180_000);
    await flushMicrotasks();
    expect(mocks.pendingUploads).toHaveBeenCalledTimes(1);

    unmount();
    expect(mocks.uploadListeners.size).toBe(0);
    expect(mocks.resumeListeners.size).toBe(0);
    await vi.advanceTimersByTimeAsync(180_000);
    await flushMicrotasks();
    expect(mocks.pendingUploads).toHaveBeenCalledTimes(1);
  });

  it("scans the captured workspace when a cross-tab upload change arrives", async () => {
    const row = upload();
    mocks.pendingUploads.mockResolvedValueOnce([]).mockResolvedValue([row]);
    mocks.deliverUpload.mockResolvedValue({ id: row.id });
    const unmount = mount();
    await flushMicrotasks();
    expect(mocks.pendingUploads).toHaveBeenCalledTimes(1);
    const changedInAnotherTab = [...mocks.uploadListeners][0];

    changedInAnotherTab();
    await flushMicrotasks();

    expect(mocks.pendingUploads.mock.calls).toEqual([
      ["/workspace/state", "workspace-1"],
      ["/workspace/state", "workspace-1"],
      ["/workspace/state", "workspace-1"],
    ]);
    expect(mocks.deliverUpload).toHaveBeenCalledWith(row);
    expect(mocks.finishUpload).toHaveBeenCalledWith(row.id);
    unmount();
  });

  it("retries the same upload after a transient IndexedDB outage longer than two minutes", async () => {
    const row = upload();
    mocks.pendingUploads.mockResolvedValue([row]);
    mocks.deliverUpload
      .mockRejectedValueOnce(new DOMException("Aborted", "AbortError"))
      .mockRejectedValueOnce(new DOMException("Aborted", "AbortError"))
      .mockRejectedValueOnce(new DOMException("Aborted", "AbortError"))
      .mockRejectedValueOnce(new DOMException("Aborted", "AbortError"))
      .mockRejectedValueOnce(new DOMException("Aborted", "AbortError"))
      .mockRejectedValueOnce(new DOMException("Aborted", "AbortError"))
      .mockRejectedValueOnce(new DOMException("Aborted", "AbortError"))
      .mockRejectedValueOnce(new DOMException("Aborted", "AbortError"))
      .mockResolvedValue({ id: row.id });
    const unmount = mount();
    await flushMicrotasks();
    expect(mocks.deliverUpload).toHaveBeenCalledTimes(1);

    await vi.advanceTimersByTimeAsync(180_000);
    await flushMicrotasks();

    expect(mocks.deliverUpload).toHaveBeenCalledTimes(9);
    expect(mocks.deliverUpload.mock.calls.map(([item]) => item.id)).toEqual(
      Array(9).fill(row.id),
    );
    expect(mocks.finishUpload).toHaveBeenCalledWith(row.id);
    expect(vi.getTimerCount()).toBe(0);
    unmount();
  });
});

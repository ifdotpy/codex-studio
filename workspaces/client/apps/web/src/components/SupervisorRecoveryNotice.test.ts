import { afterEach, expect, it, vi } from "vitest";
import type { components } from "../generated/api";
import type { ResourceRef, ResourceVersion } from "../sync/resourceEvents";
import type { DesktopNotice } from "./desktopNotices";

const mocks = vi.hoisted(() => ({
  effects: [] as (() => void | (() => void))[],
  changes: new Set<(version?: ResourceVersion) => void>(),
  notices: [] as DesktopNotice[],
  get: vi.fn(),
}));
vi.mock("react", () => ({
  useEffect: (effect: () => void | (() => void)) => mocks.effects.push(effect),
  useState: () => [
    mocks.notices,
    (notices: DesktopNotice[]) => {
      mocks.notices = notices;
    },
  ],
}));
vi.mock("../servers/ServerServicesContext", async () => {
  const { watchResourceReads } = await import("./watchResourceReads");
  return {
    useServerApi: () => ({ get: mocks.get }),
    useServerResourceReads: () => watchResourceReads,
  };
});
vi.mock("../sync/resourceEvents", () => ({
  watchResourceChanges: (
    resource: ResourceRef,
    callback: (version?: ResourceVersion) => void,
  ) => {
    expect(resource).toEqual({ kind: "desktop" });
    mocks.changes.add(callback);
    return () => mocks.changes.delete(callback);
  },
  watchResourceConnection: () => () => {},
}));
vi.mock("../sync/resume", () => ({ onResume: () => () => {} }));

import SupervisorRecoveryNotice from "./SupervisorRecoveryNotice";

let cleanup: (() => void) | undefined;
afterEach(() => {
  cleanup?.();
  cleanup = undefined;
  mocks.effects.length = 0;
  mocks.notices = [];
  mocks.get.mockReset();
  expect(mocks.changes.size).toBe(0);
});

it("reads once for a stable desktop baseline and retains a full notice on read failure", async () => {
  type Status = Pick<
    components["schemas"]["DesktopResponse"],
    "diskSpace" | "supervisorNotice"
  >;
  const healthy: Status = {
    diskSpace: { level: null, message: null },
    supervisorNotice: null,
  };
  mocks.get.mockResolvedValue(healthy);
  SupervisorRecoveryNotice();
  cleanup = mocks.effects[0]() || undefined;
  const notify = (revision: number) => {
    for (const callback of mocks.changes)
      callback({ epoch: "desktop", revision });
  };
  notify(1);
  await vi.waitFor(() => expect(mocks.get).toHaveBeenCalledTimes(1));
  expect(mocks.get.mock.calls[0][0]).toBe("/api/desktop");
  mocks.get.mockClear();
  // The server's equal-content filter retains this baseline revision on healthy disk writes.
  notify(1);
  await Promise.resolve();
  expect(mocks.get).not.toHaveBeenCalled();
  for (const [revision, level] of [
    [2, "warning"],
    [3, "critical"],
    [4, "full"],
  ] as const) {
    const status: Status = {
      diskSpace: { level, message: `Disk ${level}.` },
      supervisorNotice: "Supervisor stopped.",
    };
    mocks.get.mockResolvedValue(status);
    notify(revision);
    await vi.waitFor(() =>
      expect(mocks.notices).toEqual([
        { id: "disk", level, text: `Disk ${level}.` },
        { id: "supervisor", level: "warning", text: "Supervisor stopped." },
      ]),
    );
  }
  mocks.get.mockRejectedValue(new Error("unavailable"));
  notify(5);
  await vi.waitFor(() => expect(mocks.get).toHaveBeenCalledTimes(4));
  expect(mocks.notices[0]).toEqual({
    id: "disk",
    level: "full",
    text: "Disk full.",
  });
});

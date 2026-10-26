import { afterEach, expect, it, vi } from "vitest";
import type { Snapshot } from "../types";
import type { DesktopNotice } from "./desktopNotices";
const mocks = vi.hoisted(() => ({
  effects: [] as (() => void | (() => void))[],
  accept: new Set<(snapshot: Snapshot | null) => void>(),
  report: new Set<(error: unknown) => void>(),
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
vi.mock("../servers/ServerServicesContext", () => ({
  useServerSync: () => ({
    subscribeStateProjection: (
      accept: (snapshot: Snapshot | null) => void,
      report: (error: unknown) => void,
    ) => {
      mocks.accept.add(accept);
      mocks.report.add(report);
      return () => {
        mocks.accept.delete(accept);
        mocks.report.delete(report);
      };
    },
  }),
}));
import SupervisorRecoveryNotice from "./SupervisorRecoveryNotice";
let cleanup: (() => void) | undefined;
afterEach(() => {
  cleanup?.();
  cleanup = undefined;
  mocks.effects.length = 0;
  mocks.notices = [];
  expect(mocks.accept.size).toBe(0);
  expect(mocks.report.size).toBe(0);
});
it("renders desktop stream notices and retains the full-disk notice on projection failure", () => {
  SupervisorRecoveryNotice();
  cleanup = mocks.effects[0]() || undefined;
  for (const level of ["warning", "critical", "full"] as const) {
    for (const accept of mocks.accept)
      accept({
        runtime: {
          desktop: {
            diskSpace: { level, message: `Disk ${level}.` },
            supervisorNotice: "Supervisor stopped.",
          },
        },
      } as Snapshot);
    expect(mocks.notices).toEqual([
      { id: "disk", level, text: `Disk ${level}.` },
      { id: "supervisor", level: "warning", text: "Supervisor stopped." },
    ]);
  }
  for (const report of mocks.report) report(new Error("unavailable"));
  expect(mocks.notices[0]).toEqual({
    id: "disk",
    level: "full",
    text: "Disk full.",
  });
  expect(mocks.get).not.toHaveBeenCalled();
});

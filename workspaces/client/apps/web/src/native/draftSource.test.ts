import { expect, it, vi } from "vitest";

vi.mock("./useNativeDrafts", () => ({
  nativeDraftsHook: (core: { server: string }) => () => core.server,
}));
vi.mock("./unavailableDrafts", () => ({
  useUnavailableDrafts: () => "unavailable",
}));
import type { CoreClient } from "./client";
import { draftsForServer } from "./draftSource";

it("uses the core of each server and no drafts without a core", () => {
  const core = (server: string): CoreClient => ({
    server,
    request: async <T>(): Promise<T> => {
      throw new Error("unused");
    },
    listen: async () => () => {},
  });
  expect(draftsForServer(core("local"))()).toBe("local");
  expect(draftsForServer(core("server-a"))()).toBe("server-a");
  expect(draftsForServer(undefined)()).toBe("unavailable");
});

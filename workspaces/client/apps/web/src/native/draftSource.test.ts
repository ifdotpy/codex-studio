import { expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({ native: false }));
vi.mock("./client", () => ({ hasNativeClient: () => mocks.native }));
vi.mock("./useNativeDrafts", () => ({ useNativeDrafts: () => "native" }));
vi.mock("./unavailableDrafts", () => ({
  useUnavailableDrafts: () => "unavailable",
}));
import { draftsForServer } from "./draftSource";

it("uses the core only for the local server of the native client", () => {
  mocks.native = true;
  expect(draftsForServer(true)()).toBe("native");
  expect(draftsForServer(false)()).toBe("unavailable");
  mocks.native = false;
  expect(draftsForServer(true)()).toBe("unavailable");
});

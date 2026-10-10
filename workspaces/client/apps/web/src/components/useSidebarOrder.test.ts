import { describe, expect, it } from "vitest";
import { sidebarOrderCanWrite } from "./useSidebarOrder";

describe("sidebar order write readiness", () => {
  it("routes combined writes through the online owner frame", () => {
    expect(sidebarOrderCanWrite(undefined, true)).toBe(true);
  });

  it("requires a session token for the classic local backend", () => {
    expect(sidebarOrderCanWrite(undefined, undefined)).toBe(false);
  });

  it("allows writes with a token when availability is not tracked", () => {
    expect(sidebarOrderCanWrite("local-session", undefined)).toBe(true);
  });

  it("blocks writes when the owning server is offline", () => {
    expect(sidebarOrderCanWrite("owner-session", false)).toBe(false);
  });
});

import { describe, expect, it } from "vitest";
import { frontendIsLocal } from "./localFrontend";

describe("local frontend", () => {
  it("is local only for the local owner on this machine", () => {
    expect(frontendIsLocal("local", false, "127.0.0.1")).toBe(true);
    expect(frontendIsLocal("local", false, "studio-abc.localhost")).toBe(true);
    expect(frontendIsLocal("local", false, "remote-fixture.localhost")).toBe(
      false,
    );
    expect(frontendIsLocal("local", true, "")).toBe(true);
    expect(frontendIsLocal("local", false, "phone.example.ts.net")).toBe(false);
    expect(frontendIsLocal("serverabc", true, "localhost")).toBe(false);
  });
});

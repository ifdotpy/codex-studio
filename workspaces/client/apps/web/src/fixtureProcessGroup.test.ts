import { afterEach, expect, test, vi } from "vitest";
import { stopFixtureProcessGroup } from "../tests/fixture-process-group.ts";

afterEach(() => vi.restoreAllMocks());

test("cleanup confirms group disappearance after a denied signal", async () => {
  const denied = Object.assign(new Error("kill EPERM"), { code: "EPERM" });
  const gone = Object.assign(new Error("kill ESRCH"), { code: "ESRCH" });
  const kill = vi.spyOn(process, "kill").mockImplementation((_pid, signal) => {
    throw signal === 0 ? gone : denied;
  });

  await expect(stopFixtureProcessGroup(12345, 0)).resolves.toBeUndefined();
  expect(kill.mock.calls.map((call) => call[1])).toContain(0);
  expect(kill.mock.calls.map((call) => call[1])).not.toContain("SIGKILL");
});

test("cleanup preserves a permission error when the group still exists", async () => {
  const denied = Object.assign(new Error("kill EPERM"), { code: "EPERM" });
  const kill = vi.spyOn(process, "kill").mockImplementation(() => {
    throw denied;
  });

  await expect(stopFixtureProcessGroup(12345, 0)).rejects.toBe(denied);
  expect(kill.mock.calls.map((call) => call[1])).toContain(0);
  expect(kill.mock.calls.map((call) => call[1])).not.toContain("SIGKILL");
});

test("cleanup still escalates a live group that ignores SIGTERM", async () => {
  const gone = Object.assign(new Error("kill ESRCH"), { code: "ESRCH" });
  let killed = false;
  const kill = vi.spyOn(process, "kill").mockImplementation((_pid, signal) => {
    if (killed) throw gone;
    if (signal === "SIGKILL") killed = true;
    return true;
  });

  await expect(stopFixtureProcessGroup(12345, 0)).resolves.toBeUndefined();
  expect(kill.mock.calls.map((call) => call[1])).toContain("SIGKILL");
});

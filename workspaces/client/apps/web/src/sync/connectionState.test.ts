import { expect, it } from "vitest";
import { activityOf, healthOf, type ConnectionState } from "./connectionState";

it("derives coarse views from each connection phase", () => {
  const cases: Array<[ConnectionState, string, string]> = [
    [{ phase: "stopped" }, "connecting", "stopped"],
    [
      { phase: "connecting", attempt: 1, activity: "preparing" },
      "connecting",
      "preparing",
    ],
    [{ phase: "live", since: 1 }, "live", "open"],
    [
      {
        phase: "reconnecting",
        attempt: 2,
        cause: "stream-error",
        activity: "retry-scheduled",
      },
      "degraded",
      "retry-scheduled",
    ],
    [{ phase: "offline", since: 1 }, "offline", "offline"],
    [{ phase: "paused" }, "paused", "paused"],
    [{ phase: "schema-mismatch" }, "schema-mismatch", "stopped"],
  ];
  for (const [state, health, activity] of cases) {
    expect(healthOf(state)).toBe(health);
    expect(activityOf(state)).toBe(activity);
  }
});

import { describe, expect, it } from "vitest";
import { transitionConnection, type ConnectionState } from "./connectionState";

const states: ConnectionState[] = [
  { phase: "stopped" },
  { phase: "connecting", attempt: 1, activity: "connecting" },
  { phase: "live", since: 1 },
  {
    phase: "reconnecting",
    attempt: 1,
    cause: "stream-error",
    activity: "retry-scheduled",
    retryAt: 5,
  },
  { phase: "offline", since: 1 },
  { phase: "paused" },
  { phase: "schema-mismatch" },
];

describe("connection transitions", () => {
  it.each(states)("handles every event from $phase", (state) => {
    expect(transitionConnection(state, { type: "stopped" })).toEqual({
      phase: "stopped",
    });
    expect(
      transitionConnection(state, {
        type: "connecting",
        attempt: 2,
        activity: "preparing",
      }),
    ).toEqual({ phase: "connecting", attempt: 2, activity: "preparing" });
    expect(transitionConnection(state, { type: "live", at: 10 }).phase).toBe(
      "live",
    );
    expect(
      transitionConnection(state, {
        type: "reconnecting",
        attempt: 2,
        cause: "network-online",
        activity: "connecting",
      }),
    ).toEqual({
      phase: "reconnecting",
      attempt: 2,
      cause: "network-online",
      activity: "connecting",
    });
    expect(transitionConnection(state, { type: "offline", at: 10 }).phase).toBe(
      "offline",
    );
    expect(transitionConnection(state, { type: "paused" })).toEqual({
      phase: "paused",
    });
    expect(transitionConnection(state, { type: "schema-mismatch" })).toEqual({
      phase: "schema-mismatch",
    });
  });

  it("keeps the timestamp on repeated live or offline events", () => {
    expect(
      transitionConnection(
        { phase: "live", since: 1 },
        { type: "live", at: 2 },
      ),
    ).toEqual({ phase: "live", since: 1 });
    expect(
      transitionConnection(
        { phase: "offline", since: 1 },
        { type: "offline", at: 2 },
      ),
    ).toEqual({ phase: "offline", since: 1 });
  });
});

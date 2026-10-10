import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ResourceConnectionState } from "../sync/client";

const { listeners } = vi.hoisted(() => ({
  listeners: new Set<(state: ResourceConnectionState) => void>(),
}));

vi.mock("../sync/client", () => ({
  watchResourceConnection: (
    listener: (state: ResourceConnectionState) => void,
  ) => {
    listeners.add(listener);
    listener("live");
    return () => listeners.delete(listener);
  },
}));

import { watchVoiceTransport } from "./voiceTransport";

function broadcast(state: ResourceConnectionState) {
  for (const listener of listeners) listener(state);
}

describe("watchVoiceTransport", () => {
  beforeEach(() => listeners.clear());

  it("stops voice on every unavailable state without making a request", () => {
    const states: ResourceConnectionState[] = [];
    const stopVoice = vi.fn();
    const stop = watchVoiceTransport((state) => states.push(state), stopVoice);

    broadcast("connecting");
    broadcast("degraded");
    broadcast("offline");
    expect(states).toEqual(["live", "connecting", "degraded", "offline"]);
    expect(stopVoice).toHaveBeenCalledTimes(3);

    stop();
    broadcast("live");
    expect(states).toHaveLength(4);
    expect(stopVoice).toHaveBeenCalledTimes(3);
  });

  it("keeps voice open while the transport is live", () => {
    const stopVoice = vi.fn();
    const stop = watchVoiceTransport(vi.fn(), stopVoice);

    broadcast("live");
    expect(stopVoice).not.toHaveBeenCalled();
    stop();
  });
});

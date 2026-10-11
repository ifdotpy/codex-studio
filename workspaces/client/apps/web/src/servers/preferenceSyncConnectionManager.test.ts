import { describe, expect, it, vi } from "vitest";
import { createPreferenceSyncConnectionManager } from "./preferenceSyncConnectionManager";

describe("preference sync connection manager", () => {
  it("reconnects once after a fresh usable server summary", () => {
    const server = {
      id: "paired",
      origin: "https://paired.invalid",
      credentialId: "credential",
    };
    const connect = vi.fn((_server, _onSchemaMismatch) => () => {});
    const manager = createPreferenceSyncConnectionManager(connect);
    manager.updateServers([server], {});
    expect(connect).toHaveBeenCalledOnce();

    connect.mock.calls[0][1]();
    manager.updateLastSeen({ paired: 10 });
    expect(connect).toHaveBeenCalledTimes(2);
    manager.updateLastSeen({ paired: 10 });
    expect(connect).toHaveBeenCalledTimes(2);
    manager.stop();
  });
});

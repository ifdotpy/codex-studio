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

  it("keeps each server's label after startup refresh and resume", () => {
    const local = {
      id: "local",
      origin: "http://local.invalid",
      workspaceId: "local-workspace",
      label: "This computer",
    };
    const staleRemote = {
      id: "paired",
      origin: "https://paired.invalid",
      credentialId: "credential",
      workspaceId: "local-workspace",
      label: "Remote",
    };
    const labelRows = new Map([
      ["local-workspace", "This computer"],
      ["remote-workspace", "Remote"],
    ]);
    const registry = new Map<string, string>();
    const mismatch = new Map<string, () => void>();
    const connect = vi.fn((server, onSchemaMismatch) => {
      const row = labelRows.get(server.workspaceId);
      if (row) registry.set(server.id, row);
      mismatch.set(server.id, onSchemaMismatch);
      return () => {};
    });
    const manager = createPreferenceSyncConnectionManager(connect);
    manager.updateServers([local, staleRemote], {});

    mismatch.get("paired")!();
    const remote = {
      ...staleRemote,
      workspaceId: "remote-workspace",
    };
    manager.updateServers([local, remote], { paired: 10 });
    manager.updateLastSeen({ paired: 11 });

    expect(connect).toHaveBeenCalledTimes(3);
    expect(connect.mock.calls[2][0]).toBe(remote);
    expect(registry).toEqual(
      new Map([
        ["local", "This computer"],
        ["paired", "Remote"],
      ]),
    );
    manager.stop();
  });
});

import { describe, expect, it } from "vitest";
import type { ServerAccessState } from "./accessContract";
import { discoverySnapshot } from "./discoveryModel";

describe("automatic update setting", () => {
  const state = (
    settings: ServerAccessState["settings"],
  ): ServerAccessState => ({
    protocol: 1,
    identity: {
      serverId: "home",
      label: "Home",
      origin: null,
      publicKey: "key",
      tailscaleUser: null,
    },
    clients: [],
    settings,
    servers: [],
    invites: [],
  });

  it("reads the server setting and treats an older server as on", () => {
    expect(
      discoverySnapshot(state({ autoPair: true, autoUpdate: false }))
        ?.autoUpdate,
    ).toBe(false);
    expect(discoverySnapshot(state({ autoPair: true }))?.autoUpdate).toBe(true);
  });
});

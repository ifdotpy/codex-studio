import { describe, expect, it, vi } from "vitest";
import type { StudioServer } from "./registry";
const state = vi.hoisted(() => ({
  order: ["remote-workspace", "local-workspace"],
}));
vi.mock("../sync/uiPreferenceStore", () => ({
  readPreferenceFields: () => ({ '["server-order"]': { value: state.order } }),
}));
import { orderPreferenceServers } from "./preferenceServerOrder";
describe("server preference order", () => {
  it("places the local server at its shared position and retains unknown server order", () => {
    const servers = [
      { id: "local", workspaceId: "local-workspace" },
      { id: "remote", workspaceId: "remote-workspace" },
      { id: "new", workspaceId: "new-workspace" },
    ] as StudioServer[];
    expect(orderPreferenceServers(servers).map((server) => server.id)).toEqual([
      "remote",
      "local",
      "new",
    ]);
    expect(servers[0].id).toBe("local");
  });
});

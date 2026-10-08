import { describe, expect, it } from "vitest";
import { relativeSeen, serverRows } from "./serverRows";
import type { DiscoveredServer } from "./discoveryModel";
const peer = (
  id: string,
  status: DiscoveredServer["status"],
): DiscoveredServer => ({
  id,
  label: id,
  origin: `https://${id}.tailnet.ts.net`,
  status,
  lastSeen: null,
  paired: status === "paired",
  publicKey: "key",
  generation: "1",
});
describe("Settings server rows", () => {
  it("shows each server once in local, active, discovered, unreachable, revoked order", () => {
    const rows = serverRows(
      [
        {
          id: "remote",
          label: "remote",
          origin: "https://remote.tailnet.ts.net",
        },
        { id: "local", label: "z-local", origin: "http://localhost:4444" },
        {
          id: "revoked",
          label: "revoked",
          origin: "https://revoked.tailnet.ts.net",
        },
      ],
      [
        peer("revoked", "revoked"),
        peer("offline", "unreachable"),
        peer("new", "discovered"),
        peer("remote", "paired"),
      ],
      { remote: "live", revoked: "live" },
    );
    expect(rows.map((row) => row.server.id)).toEqual([
      "local",
      "remote",
      "new",
      "offline",
      "revoked",
    ]);
    expect(rows.map((row) => row.status)).toEqual([
      "Online",
      "Active",
      "Discovered",
      "Unreachable",
      "Revoked",
    ]);
    expect(rows.at(-1)?.registered?.id).toBe("revoked");
  });
  it("keeps a UI-only connection distinct from local server peer permission", () => {
    const [row] = serverRows(
      [
        {
          id: "manual",
          label: "manual",
          origin: "https://manual.tailnet.ts.net",
        },
      ],
      [],
      { manual: "offline" },
    );
    expect(row.status).toBe("Offline");
    expect(row.peer).toBeUndefined();
    expect(row.registered?.id).toBe("manual");
  });
  it("uses relative last contact time and clamps future clock skew", () => {
    expect(relativeSeen(1001, 1000)).toBe("just now");
    expect(relativeSeen(880, 1000)).toBe("2 min ago");
    expect(relativeSeen(1000, 8200)).toBe("2 h ago");
    expect(relativeSeen(1000, 173800)).toBe("2 d ago");
  });
});

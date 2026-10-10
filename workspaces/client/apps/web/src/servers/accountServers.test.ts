import { expect, it } from "vitest";
import { accountServers } from "./accountServers";
import type { DiscoveredServer } from "./discoveryModel";

function peer(
  id: string,
  status: DiscoveredServer["status"],
): DiscoveredServer {
  return {
    id,
    label: id,
    origin: `https://${id}.tailnet.ts.net`,
    status,
    paired: status === "paired",
    lastSeen: null,
    publicKey: "public-key",
    generation: "1",
  };
}

it("includes backend peers without UI credentials and preserves a registered credential once", () => {
  const registered = {
    ...peer("remote", "paired"),
    credentialId: "ui-credential",
  };
  const rows = accountServers(
    [
      { id: "local", label: "This computer", origin: "http://localhost" },
      registered,
    ],
    [
      peer("remote", "paired"),
      peer("backend", "paired"),
      peer("offline", "unreachable"),
      peer("new", "discovered"),
      peer("revoked", "revoked"),
    ],
  );
  expect(rows.map((row) => row.id)).toEqual([
    "local",
    "remote",
    "backend",
    "offline",
  ]);
  expect(rows[1]).toBe(registered);
  expect(rows[2].credentialId).toBeUndefined();
});

it("removes a backend-revoked peer even if this UI still has its credential", () => {
  const registered = {
    ...peer("remote", "paired"),
    credentialId: "ui-credential",
  };
  expect(accountServers([registered], [peer("remote", "revoked")])).toEqual([]);
});

it("keeps the standalone local and manually paired UI server list", () => {
  const registered = [
    { id: "local", label: "This computer", origin: "http://localhost" },
    { ...peer("manual", "paired"), credentialId: "ui-credential" },
  ];
  expect(accountServers(registered, [])).toEqual(registered);
});

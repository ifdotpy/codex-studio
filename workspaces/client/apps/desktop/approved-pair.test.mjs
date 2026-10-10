import { expect, test, vi } from "vitest";
import { createRequire } from "node:module";
const { pairApproved } = createRequire(import.meta.url)("./approved-pair.cjs");
function fixture() {
  const invitation = {
    protocol: 1,
    serverId: "remote",
    origin: "https://remote.tailnet.ts.net",
    publicKey: "peer-key",
    inviteId: "invite",
    token: "secret",
    tailscaleUser: "owner",
    expires: 1000,
  };
  const peer = { ...invitation, kind: "server", status: "paired", created: 1 };
  const state = {
    protocol: 1,
    identity: { serverId: "home" },
    servers: [peer],
  };
  const value = {
    origin: invitation.origin,
    invitation,
    requestId: "pair-id",
    approval: {
      localServerId: "home",
      serverId: "remote",
      generation: JSON.stringify([1, "peer-key"]),
      inviteRequestId: "invite-id",
    },
  };
  const credentials = { pair: vi.fn(async () => ({ id: "remote" })) };
  const fetchRequest = vi.fn(async (url, init) => {
    expect(new URL(url).origin).toBe("http://127.0.0.1:4620");
    expect(init.redirect).toBe("error");
    if (url.endsWith("/api/session"))
      return Response.json({ token: "local-secret" });
    if (init.method === "POST") {
      expect(init.headers["X-Canvas-Token"]).toBe("local-secret");
      expect(JSON.parse(init.body)).toEqual({
        action: "ui_invite",
        serverId: "remote",
        requestId: "invite-id",
      });
      return Response.json({ invitation });
    }
    return Response.json(state);
  });
  const trusted = vi.fn();
  const run = () =>
    pairApproved({
      origin: "http://127.0.0.1:4620",
      value,
      credentials,
      fetchRequest,
      trusted,
    });
  return {
    invitation,
    peer,
    state,
    value,
    credentials,
    fetchRequest,
    trusted,
    run,
  };
}
test("automatic access verifies the local peer and the saved invitation before using its exact pair ID", async () => {
  const f = fixture();
  expect(await f.run()).toEqual({ id: "remote" });
  expect(f.credentials.pair).toHaveBeenCalledExactlyOnceWith({
    origin: f.value.origin,
    invitation: f.invitation,
    requestId: "pair-id",
  });
  expect(f.fetchRequest).toHaveBeenCalledTimes(4);
  expect(f.trusted).toHaveBeenCalledTimes(8);
});
test.each(["revoked", "origin", "key", "generation", "home", "kind"])(
  "automatic access rejects changed %s before pairing",
  async (change) => {
    const f = fixture();
    if (change === "revoked") f.peer.status = "revoked";
    if (change === "origin") f.peer.origin = "https://other.tailnet.ts.net";
    if (change === "key") f.peer.publicKey = "changed";
    if (change === "generation") f.peer.created = 2;
    if (change === "home") f.state.identity.serverId = "another-home";
    if (change === "kind") f.peer.kind = "ui";
    await expect(f.run()).rejects.toThrow("not approved");
    expect(f.credentials.pair).not.toHaveBeenCalled();
  },
);
test("automatic access rejects a renderer invitation that differs from its local server receipt", async () => {
  const f = fixture();
  f.value.invitation = { ...f.invitation, token: "another-secret" };
  await expect(f.run()).rejects.toThrow("does not match");
  expect(f.credentials.pair).not.toHaveBeenCalled();
});
test("automatic access checks revocation after the invitation returns", async () => {
  const f = fixture();
  const original = f.fetchRequest.getMockImplementation();
  f.fetchRequest.mockImplementation(async (url, init) => {
    const response = await original(url, init);
    if (init.method === "POST") f.peer.status = "revoked";
    return response;
  });
  await expect(f.run()).rejects.toThrow("not approved");
  expect(f.credentials.pair).not.toHaveBeenCalled();
});

import { expect, it, vi } from "vitest";
import {
  AutomaticUiAccess,
  type AutomaticAccessAttempt,
} from "./automaticUiAccess";
import type { DiscoverySnapshot } from "./discoveryModel";
const invitation = {
  protocol: 1 as const,
  inviteId: "invite",
  token: "secret",
  serverId: "remote",
  label: "Remote",
  origin: "https://remote.tailnet.ts.net",
  publicKey: "-----BEGIN PUBLIC KEY-----\nabc\n-----END PUBLIC KEY-----\n",
  tailscaleUser: "owner",
  expires: 1000,
};
const server = {
  id: "remote",
  origin: invitation.origin,
  label: "Remote",
  credentialId: "ui-key",
};
const snapshot: DiscoverySnapshot = {
  localServerId: "home",
  autoPair: true,
  servers: [
    {
      id: "remote",
      label: "Remote",
      origin: invitation.origin,
      paired: true,
      status: "paired",
      lastSeen: 1,
      publicKey: invitation.publicKey,
      generation: "generation1",
    },
  ],
};
function setup() {
  const records = new Map<string, AutomaticAccessAttempt>();
  const existing: (typeof server)[] = [];
  const adapter = {
    current: vi.fn(async () => snapshot),
    invite: vi.fn(
      async (_peer: DiscoverySnapshot["servers"][number], _requestId: string) =>
        invitation,
    ),
    pair: vi.fn(
      async (_origin: string, _code: string, _requestId: string) => server,
    ),
    existing: () => existing,
    excluded: vi.fn(() => false),
    add: vi.fn((value: typeof server) => existing.push(value)),
  };
  const store = {
    read: vi.fn(async (key: string) => records.get(key) ?? null),
    save: vi.fn(async (key: string, value: AutomaticAccessAttempt) => {
      records.set(key, structuredClone(value));
    }),
    remove: vi.fn(async (key: string) => {
      records.delete(key);
    }),
  };
  return {
    records,
    adapter,
    store,
    controller: new AutomaticUiAccess(store, adapter, () => 100000),
    existing,
  };
}
it("adds a paired server once after saving both identities and its invitation", async () => {
  const s = setup();
  await s.controller.reconcile(snapshot);
  expect(s.adapter.invite).toHaveBeenCalledOnce();
  expect(s.store.save).toHaveBeenCalledTimes(3);
  expect(s.store.save.mock.invocationCallOrder[0]).toBeLessThan(
    s.adapter.invite.mock.invocationCallOrder[0],
  );
  expect(s.store.save.mock.invocationCallOrder[1]).toBeLessThan(
    s.adapter.pair.mock.invocationCallOrder[0],
  );
  expect(s.existing).toEqual([server]);
  expect(s.records.size).toBe(0);
  await s.controller.reconcile(snapshot);
  expect(s.adapter.pair).toHaveBeenCalledOnce();
});
it("retries a lost invitation response with the same saved request identity after reload", async () => {
  const s = setup();
  s.adapter.invite.mockRejectedValueOnce(new TypeError("Response lost"));
  await expect(s.controller.reconcile(snapshot)).rejects.toThrow(
    "Response lost",
  );
  await new AutomaticUiAccess(s.store, s.adapter, () => 100000).reconcile(
    snapshot,
  );
  expect(s.adapter.invite.mock.calls[0]).toEqual(
    s.adapter.invite.mock.calls[1],
  );
  expect(s.existing).toEqual([server]);
});
it("retries lost pairing with the same invitation and pair identity after reload", async () => {
  const s = setup();
  s.adapter.pair.mockRejectedValueOnce(new TypeError("Pair response lost"));
  await expect(s.controller.reconcile(snapshot)).rejects.toThrow(
    "Pair response lost",
  );
  await new AutomaticUiAccess(s.store, s.adapter, () => 100000).reconcile(
    snapshot,
  );
  expect(s.adapter.invite).toHaveBeenCalledTimes(2);
  expect(s.adapter.invite.mock.calls[0]).toEqual(
    s.adapter.invite.mock.calls[1],
  );
  expect(s.adapter.pair.mock.calls[0]).toEqual(s.adapter.pair.mock.calls[1]);
  expect(s.existing).toEqual([server]);
});
it("never requests or adds a revoked server", async () => {
  const s = setup();
  await s.controller.reconcile({
    ...snapshot,
    servers: [{ ...snapshot.servers[0], status: "revoked", paired: false }],
  });
  expect(s.adapter.invite).not.toHaveBeenCalled();
  expect(s.adapter.pair).not.toHaveBeenCalled();
});
it("retains an offline attempt and bounds retries without adding a server", async () => {
  const s = setup();
  s.adapter.invite.mockRejectedValue(new TypeError("Offline"));
  await expect(s.controller.reconcile(snapshot)).rejects.toThrow("Offline");
  await expect(s.controller.reconcile(snapshot)).rejects.toThrow("Offline");
  expect(s.adapter.invite).toHaveBeenCalledOnce();
  expect(s.records.size).toBe(1);
  expect(s.existing).toEqual([]);
});
it("does not pair if storage fails before the invitation can be saved", async () => {
  const s = setup();
  s.store.save.mockRejectedValue(new Error("Storage full"));
  await expect(s.controller.reconcile(snapshot)).rejects.toThrow(
    "Storage full",
  );
  expect(s.adapter.invite).not.toHaveBeenCalled();
});
it("rejects an invitation for another server", async () => {
  const s = setup();
  s.adapter.invite.mockResolvedValue({ ...invitation, serverId: "other" });
  await expect(s.controller.reconcile(snapshot)).rejects.toThrow(
    "another server",
  );
  expect(s.adapter.pair).not.toHaveBeenCalled();
});

it("keeps an explicit UI removal until the user adds it again", async () => {
  const s = setup();
  s.adapter.excluded.mockReturnValue(true);
  await s.controller.reconcile(snapshot);
  expect(s.adapter.pair).not.toHaveBeenCalled();
  s.adapter.excluded.mockReturnValue(false);
  await s.controller.reconcile(snapshot);
  expect(s.existing).toEqual([server]);
});
it("pins the invitation public key to the local peer snapshot", async () => {
  const s = setup();
  s.adapter.invite.mockResolvedValue({
    ...invitation,
    publicKey: "-----BEGIN PUBLIC KEY-----\nother\n-----END PUBLIC KEY-----\n",
  });
  await expect(s.controller.reconcile(snapshot)).rejects.toThrow(
    "another server",
  );
  expect(s.adapter.pair).not.toHaveBeenCalled();
});

it("rechecks revocation before pairing an invitation returned late", async () => {
  const s = setup();
  let finish!: (value: typeof invitation) => void;
  s.adapter.invite.mockImplementationOnce(
    () =>
      new Promise((resolve) => {
        finish = resolve;
      }),
  );
  const active = s.controller.reconcile(snapshot);
  await vi.waitFor(() => expect(s.adapter.invite).toHaveBeenCalledOnce());
  const revoked = {
    ...snapshot,
    servers: [
      { ...snapshot.servers[0], status: "revoked" as const, paired: false },
    ],
  };
  const update = s.controller.reconcile(revoked);
  finish(invitation);
  await Promise.all([active, update]);
  expect(s.adapter.pair).not.toHaveBeenCalled();
});
it("does not register UI access after its controller stops", async () => {
  const s = setup();
  let finish!: (value: typeof server) => void;
  s.adapter.pair.mockImplementationOnce(
    () =>
      new Promise((resolve) => {
        finish = resolve;
      }),
  );
  const active = s.controller.reconcile(snapshot);
  await vi.waitFor(() => expect(s.adapter.pair).toHaveBeenCalledOnce());
  s.controller.stop();
  finish(server);
  await active;
  expect(s.adapter.add).not.toHaveBeenCalled();
});

it("stores no invitation token after an unknown pair result", async () => {
  const s = setup();
  s.adapter.pair.mockRejectedValue(new TypeError("Pair response lost"));
  await expect(s.controller.reconcile(snapshot)).rejects.toThrow(
    "Pair response lost",
  );
  expect(JSON.stringify([...s.records.values()])).not.toContain('"token"');
  expect(JSON.stringify([...s.records.values()])).not.toContain("secret");
});
it("renews an expired invitation that never started pairing", async () => {
  const s = setup();
  const key = JSON.stringify([
    "home",
    "remote",
    invitation.origin,
    "generation1",
  ]);
  s.records.set(key, {
    localServerId: "home",
    serverId: "remote",
    origin: invitation.origin,
    generation: "generation1",
    inviteRequestId: "expired-invite-request",
    pairRequestId: "expired-pair-request",
    invitation: { ...invitation, expires: 90 },
  });
  await s.controller.reconcile(snapshot);
  expect(s.adapter.invite).toHaveBeenCalledOnce();
  expect(s.adapter.invite.mock.calls[0][1]).not.toBe("expired-invite-request");
  expect(s.adapter.pair.mock.calls[0][2]).not.toBe("expired-pair-request");
});
it("reads authoritative peer state before pairing and before registration", async () => {
  const s = setup();
  const current = vi.fn(async () => ({
    ...snapshot,
    servers: [
      { ...snapshot.servers[0], status: "revoked" as const, paired: false },
    ],
  }));
  await new AutomaticUiAccess(
    s.store,
    { ...s.adapter, current },
    () => 100000,
  ).reconcile(snapshot);
  expect(s.adapter.pair).not.toHaveBeenCalled();
  expect(s.adapter.add).not.toHaveBeenCalled();
});

it("does not add a peer revoked while pairing was in flight", async () => {
  const s = setup();
  s.adapter.current.mockResolvedValueOnce(snapshot).mockResolvedValueOnce({
    ...snapshot,
    servers: [{ ...snapshot.servers[0], status: "revoked", paired: false }],
  });
  await s.controller.reconcile(snapshot);
  expect(s.adapter.pair).toHaveBeenCalledOnce();
  expect(s.adapter.add).not.toHaveBeenCalled();
});
it("cancels a held invitation immediately when this UI revokes the peer", async () => {
  const s = setup();
  let finish!: (value: typeof invitation) => void;
  s.adapter.invite.mockImplementationOnce(
    () =>
      new Promise((resolve) => {
        finish = resolve;
      }),
  );
  const active = s.controller.reconcile(snapshot);
  await vi.waitFor(() => expect(s.adapter.invite).toHaveBeenCalledOnce());
  s.controller.cancel("remote");
  finish(invitation);
  await active;
  expect(s.adapter.pair).not.toHaveBeenCalled();
  expect(s.adapter.add).not.toHaveBeenCalled();
});
it("keeps the same identity when an uncertain pair outlives its invitation", async () => {
  const s = setup();
  s.adapter.pair.mockRejectedValueOnce(new TypeError("Pair response lost"));
  await expect(s.controller.reconcile(snapshot)).rejects.toThrow(
    "Pair response lost",
  );
  await new AutomaticUiAccess(s.store, s.adapter, () => 2000000).reconcile(
    snapshot,
  );
  expect(s.adapter.invite.mock.calls[0]).toEqual(
    s.adapter.invite.mock.calls[1],
  );
  expect(s.adapter.pair.mock.calls[0]).toEqual(s.adapter.pair.mock.calls[1]);
});

it("does not mark pairing as uncertain when its final peer check fails before the send", async () => {
  const s = setup();
  s.adapter.current.mockRejectedValueOnce(new TypeError("Home server offline"));
  await expect(s.controller.reconcile(snapshot)).rejects.toThrow(
    "Home server offline",
  );
  expect(s.adapter.pair).not.toHaveBeenCalled();
  expect([...s.records.values()][0].pairStarted).toBe(false);
});

it("renews an invitation that expires during its final peer check without sending a pair", async () => {
  const s = setup();
  let time = 100000;
  s.adapter.current.mockImplementationOnce(async () => {
    time = 1100000;
    return snapshot;
  });
  await expect(
    new AutomaticUiAccess(s.store, s.adapter, () => time).reconcile(snapshot),
  ).rejects.toThrow("expired before pairing");
  expect(s.adapter.pair).not.toHaveBeenCalled();
  expect(s.records.size).toBe(0);
  s.adapter.invite.mockResolvedValueOnce({ ...invitation, expires: 2000 });
  await new AutomaticUiAccess(s.store, s.adapter, () => time).reconcile(
    snapshot,
  );
  expect(s.adapter.invite.mock.calls[0][1]).not.toBe(
    s.adapter.invite.mock.calls[1][1],
  );
  expect(s.adapter.pair).toHaveBeenCalledOnce();
});
it("explicit Add to this UI clears the local cancellation generation", async () => {
  const s = setup();
  s.adapter.excluded.mockReturnValue(true);
  await s.controller.reconcile(snapshot);
  s.controller.cancel("remote");
  s.controller.retry("remote");
  s.adapter.excluded.mockReturnValue(false);
  await s.controller.reconcile(snapshot);
  expect(s.adapter.pair).toHaveBeenCalledOnce();
});

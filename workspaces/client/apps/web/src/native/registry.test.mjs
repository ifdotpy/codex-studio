import { afterEach, expect, it, vi } from "vitest";
import { openNativePreferences } from "./preferenceStore";
import {
  createNativeRegistry,
  installNativeRegistry,
  localServer,
  viewServer,
} from "./registry";
import { LOCAL_ALIAS_KEY } from "../servers/serverAliasModel";
import { LOCAL_NAME_KEY, SERVER_REGISTRY_KEY } from "../servers/registryModel";

afterEach(() => vi.unstubAllGlobals());
async function fixture(values = {}, selected = "local") {
  let revision = 1;
  const calls = [];
  const local = {
    id: "local",
    label: "Host device",
    origin: "http://127.0.0.1:4620",
    workspaceId: "a".repeat(32),
  };
  const peer = {
    id: "remote",
    label: "Host peer",
    origin: "https://peer.tailnet.ts.net",
    credentialId: "vault-peer",
    workspaceId: "b".repeat(32),
  };
  const selectedRow = selected === "local" ? local : peer;
  const context = {
    handle: `context:${selected}`,
    serverId: selected,
    workspaceId: selectedRow.workspaceId,
    origin: selectedRow.origin,
    credentialId: selectedRow.credentialId ?? null,
    async request(method, params, id) {
      calls.push({ method, params, id });
      if (method === "preferences.raw_all") return { ...values };
      if (method === "system.uuid") return `native-request-${calls.length}`;
      if (method === "preferences.raw_set") {
        values[params.key] = params.raw;
        return { saved: true, revision: String(++revision) };
      }
      throw new Error(`Unexpected fixture method: ${method}`);
    },
    async subscribe() {
      return async () => {};
    },
  };
  let snapshot = {
    serverId: selected,
    workspaceId: context.workspaceId,
    revision: "1",
    local,
    servers: [peer],
  };
  let receive;
  const stopped = vi.fn();
  const metadata = {
    read: vi.fn(async (owner) => {
      expect(owner).toBe(context);
      return snapshot;
    }),
    subscribe: vi.fn(async (owner, changed) => {
      expect(owner).toBe(context);
      receive = changed;
      return async () => stopped();
    }),
  };
  const storage = await openNativePreferences(context, vi.fn());
  return {
    context,
    storage,
    calls,
    metadata,
    stopped,
    get snapshot() {
      return snapshot;
    },
    emit(value) {
      snapshot = value;
      receive(value);
    },
  };
}

it("requires verified host metadata and never derives membership from preferences", async () => {
  const f = await fixture({
    [SERVER_REGISTRY_KEY]: JSON.stringify([
      {
        id: "injected",
        label: "Not a host pairing",
        origin: "https://unpaired.tailnet.ts.net",
        credentialId: "not-in-the-vault",
      },
    ]),
  });
  await expect(createNativeRegistry(f, { report: vi.fn() })).rejects.toThrow(
    /host registry metadata/i,
  );
  const registry = await createNativeRegistry(f, {
    metadata: f.metadata,
    report: vi.fn(),
  });
  expect(registry.read().map((row) => row.id)).toEqual(["remote"]);
  expect(registry.local().origin).toBe(f.context.origin);
  await registry.close();
});

it("refuses malformed host server rows and stored transport changes", async () => {
  for (const patch of [
    { id: "../other" },
    { id: "local" },
    { credentialId: undefined },
    { origin: "https://user:secret@peer.tailnet.ts.net" },
    { alias: "bad" },
  ]) {
    const f = await fixture();
    const metadata = {
      ...f.metadata,
      read: async () => ({
        ...f.snapshot,
        servers: [{ ...f.snapshot.servers[0], ...patch }],
      }),
    };
    await expect(
      createNativeRegistry(f, { metadata, report: vi.fn() }),
    ).rejects.toThrow();
  }
  const f = await fixture({
    [SERVER_REGISTRY_KEY]: JSON.stringify([
      {
        id: "remote",
        label: "Peer",
        origin: "https://other.tailnet.ts.net",
        credentialId: "vault-peer",
      },
    ]),
  });
  await expect(
    createNativeRegistry(f, { metadata: f.metadata, report: vi.fn() }),
  ).rejects.toThrow(/another host identity/);
  expect(f.stopped).toHaveBeenCalledOnce();
});

it("uses captured native preferences, awaits commit, and subscribes without browser defaults", async () => {
  vi.stubGlobal(
    "localStorage",
    new Proxy(
      {},
      {
        get() {
          throw new Error("Browser storage is forbidden");
        },
      },
    ),
  );
  vi.stubGlobal(
    "location",
    new Proxy(
      {},
      {
        get() {
          throw new Error("Browser location is forbidden");
        },
      },
    ),
  );
  const f = await fixture();
  const registry = await createNativeRegistry(f, {
    metadata: f.metadata,
    report: vi.fn(),
  });
  const changed = vi.fn();
  const stop = registry.subscribe(changed);
  f.storage.setItem(LOCAL_NAME_KEY, "My device");
  f.storage.setItem(LOCAL_ALIAS_KEY, "DEV");
  await f.storage.flush();
  expect(registry.local()).toMatchObject({ label: "My device", alias: "DEV" });
  expect(changed).toHaveBeenCalled();
  await registry.write([
    { ...registry.read()[0], label: "My peer", alias: "PEE" },
  ]);
  expect(f.storage.pending).toBe(false);
  expect(
    f.calls.filter((call) => call.method === "preferences.raw_set").at(-1)
      .params.key,
  ).toBe(SERVER_REGISTRY_KEY);
  expect(registry.read()[0].label).toBe("My peer");
  stop();
  await registry.close();
  expect(f.stopped).toHaveBeenCalledOnce();
});

it("refuses another storage owner, context scope, or host-selected transport identity", async () => {
  const a = await fixture(),
    b = await fixture();
  await expect(
    createNativeRegistry(
      { context: a.context, storage: b.storage },
      { metadata: a.metadata, report: vi.fn() },
    ),
  ).rejects.toThrow(/another owner/);
  for (const change of [
    { serverId: "other" },
    { workspaceId: "c".repeat(32) },
    { local: { ...a.snapshot.local, origin: "http://127.0.0.1:9999" } },
  ]) {
    const metadata = {
      ...a.metadata,
      read: async () => ({ ...a.snapshot, ...change }),
    };
    await expect(
      createNativeRegistry(a, { metadata, report: vi.fn() }),
    ).rejects.toThrow(/owner|identity|scope/);
  }
  const registry = await createNativeRegistry(a, {
    metadata: a.metadata,
    report: vi.fn(),
  });
  Object.assign(a.context, { workspaceId: "c".repeat(32) });
  expect(() => registry.read()).toThrow(/scope/);
  await registry.close();
});

it("cannot add, remove, or rebind a paired server through display preferences", async () => {
  const f = await fixture();
  const registry = await createNativeRegistry(f, {
    metadata: f.metadata,
    report: vi.fn(),
  });
  const peer = registry.read()[0];
  for (const rows of [
    [],
    [{ ...peer, id: "new" }],
    [{ ...peer, credentialId: "wrong" }],
    [{ ...peer, origin: "https://other.tailnet.ts.net" }],
  ])
    await expect(registry.write(rows)).rejects.toThrow(
      /host|identity|membership/,
    );
  expect(f.calls.some((call) => call.method === "preferences.raw_set")).toBe(
    false,
  );
  await registry.close();
});

it("ignores stale host callbacks and refuses equal-revision contradictions", async () => {
  const f = await fixture(),
    report = vi.fn();
  const registry = await createNativeRegistry(f, {
    metadata: f.metadata,
    report,
  });
  f.emit({
    ...f.snapshot,
    revision: "2",
    servers: [{ ...f.snapshot.servers[0], label: "New host label" }],
  });
  f.emit({ ...f.snapshot, revision: "1", servers: [] });
  expect(registry.read()[0].label).toBe("New host label");
  f.emit({ ...f.snapshot, revision: "2", servers: [] });
  expect(report).toHaveBeenCalled();
  expect(() => registry.read()).toThrow(/revision/);
  await registry.close();
});

it("native alias exports require bootstrap and retain the installed owner", async () => {
  expect(() => localServer()).toThrow(/not ready/);
  const f = await fixture({}, "remote");
  const registry = await createNativeRegistry(f, {
    metadata: f.metadata,
    report: vi.fn(),
  });
  const uninstall = installNativeRegistry(registry);
  expect(viewServer("remote").credentialId).toBe(f.context.credentialId);
  expect(() => viewServer("missing")).toThrow(/removed/);
  uninstall();
  expect(() => localServer()).toThrow(/not ready/);
  await registry.close();
});

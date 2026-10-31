import { afterEach, expect, it, vi } from "vitest";
import type { ResourceVersion } from "../sync/resourceEvents";
import type { ServerAccessState } from "./accessContract";

const mocks = vi.hoisted(() => ({
  effects: [] as (() => void | (() => void))[],
  read: undefined as undefined | ((version?: ResourceVersion) => Promise<void>),
  access: vi.fn(),
  snapshots: [] as unknown[],
  setLocalServerDetails: vi.fn(),
  stateCalls: 0,
  snapshotOverride: undefined as unknown,
  fields: {} as Record<string, unknown>,
}));
vi.mock("react", () => ({
  useEffect: (effect: () => void | (() => void)) => mocks.effects.push(effect),
  useRef: (current: unknown) => ({ current }),
  useState: (current: unknown) => [
    mocks.stateCalls++ === 0 && mocks.snapshotOverride !== undefined
      ? mocks.snapshotOverride
      : current,
    (value: unknown) => mocks.snapshots.push(value),
  ],
}));
vi.mock("../sync/uiPreferenceStore", () => ({
  readPreferenceFields: () => mocks.fields,
  capturePreferenceWrite: vi.fn(),
}));
vi.mock("./ServerServicesContext", () => ({
  useServerApi: () => ({ serverAccess: mocks.access }),
  useServerResourceReads:
    () => (_resource: unknown, read: typeof mocks.read) => {
      mocks.read = read;
      return Object.assign(() => {}, { refresh: () => read?.() });
    },
}));
vi.mock("./automaticAccessStore", () => ({
  automaticAccessStore: () => ({ initialize: async () => {} }),
  excludedServers: () => new Set(),
}));
vi.mock("./automaticUiAccess", () => ({
  AutomaticUiAccess: class {
    async reconcile() {}
    stop() {}
  },
}));
vi.mock("./registry", () => ({
  readServers: () => [],
  writeServers: () => {},
  setLocalServerDetails: mocks.setLocalServerDetails,
  LOCAL_NAME_KEY: "local-name",
  SERVER_REGISTRY_EVENT: "registry-changed",
  serveOrigin: (origin: string) => origin,
}));
vi.mock("./serverNames", () => ({
  watchRegisteredServerNames: () => () => {},
}));
vi.mock("./managementRequests", () => ({
  ServerManagementRequests: class {
    pending() {
      return [];
    }
  },
}));
import { useServerDiscovery } from "./useServerDiscovery";

let cleanups: (() => void)[] = [];
afterEach(() => {
  cleanups.forEach((cleanup) => cleanup());
  cleanups = [];
  mocks.effects.length = 0;
  mocks.snapshots.length = 0;
  mocks.access.mockReset();
  mocks.setLocalServerDetails.mockReset();
  mocks.stateCalls = 0;
  mocks.snapshotOverride = undefined;
  mocks.fields = {};
  mocks.read = undefined;
  vi.unstubAllGlobals();
});

it("uses stream snapshots without a fallback HTTP read", async () => {
  vi.stubGlobal("window", new EventTarget());
  vi.stubGlobal("location", { origin: "http://owner.invalid" });
  const setItem = vi.fn();
  vi.stubGlobal("localStorage", { getItem: () => null, setItem });
  useServerDiscovery(true, vi.fn());
  cleanups = mocks.effects.flatMap((effect) => {
    const cleanup = effect();
    return cleanup ? [cleanup] : [];
  });
  const snapshot: ServerAccessState = {
    protocol: 1,
    identity: {
      serverId: "owner",
      label: "Owner",
      origin: null,
      publicKey: "key",
      tailscaleUser: null,
    },
    clients: [],
    servers: [],
    invites: [],
    settings: { autoPair: true, aliases: { local: "OWN" } },
  };
  mocks.access.mockResolvedValue(snapshot);
  await mocks.read?.({
    epoch: "owner",
    revision: 1,
    discoverySnapshot: snapshot,
  } as ResourceVersion);
  expect(mocks.access).not.toHaveBeenCalled();
  expect(mocks.snapshots).toContainEqual(
    expect.objectContaining({ localServerId: "owner" }),
  );
  expect(mocks.setLocalServerDetails).toHaveBeenCalledWith({
    label: "Owner",
    alias: "OWN",
  });
  expect(setItem).not.toHaveBeenCalled();
  await mocks.read?.();
  expect(mocks.access).not.toHaveBeenCalled();
  const newer = {
    ...snapshot,
    identity: { ...snapshot.identity, label: "New name" },
  };
  await mocks.read?.({
    epoch: "owner",
    revision: 2,
    discoverySnapshot: newer,
  } as ResourceVersion);
  const snapshots = mocks.snapshots.filter(
    (value): value is { localLabel: string } =>
      !!value && typeof value === "object" && "localLabel" in value,
  );
  expect(snapshots.at(-1)?.localLabel).toBe("New name");
  mocks.snapshotOverride = snapshots.at(-1);
  mocks.fields = { '["server-alias:workspace"]': { value: "NEW" } };
  mocks.stateCalls = 0;
  const effectsBefore = mocks.effects.length;
  useServerDiscovery(true, vi.fn(), true, { local: "workspace" });
  for (const effect of mocks.effects.slice(effectsBefore)) {
    const cleanup = effect();
    if (cleanup) cleanups.push(cleanup);
  }
  expect(mocks.setLocalServerDetails).toHaveBeenCalledWith({ alias: "NEW" });
});

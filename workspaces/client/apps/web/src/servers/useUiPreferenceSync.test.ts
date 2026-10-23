import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { preferenceCacheKey } from "../sync/uiPreferenceMerge";
import { SERVER_REGISTRY_KEY } from "./registry";

const mocks = vi.hoisted(() => ({
  effects: [] as (() => void | (() => void))[],
  streams: [] as EventTarget[],
  fetch: vi.fn(async () => Response.json({})),
}));
vi.mock("react", () => ({
  useEffect: (effect: () => void | (() => void)) => mocks.effects.push(effect),
  useRef: (current: unknown) => ({ current }),
}));
vi.mock("./ServerServicesContext", () => ({
  useServerApi: () => ({ fetchApiRequest: mocks.fetch }),
}));
vi.mock("./eventSource", () => ({
  SignedEventSource: class extends EventTarget {
    constructor() {
      super();
      mocks.streams.push(this);
    }
    close() {}
  },
}));
import { useUiPreferenceSync } from "./useUiPreferenceSync";

const local = { id: "local", origin: "http://local.invalid", label: "Local" };
const paired = {
  id: "paired",
  origin: "https://paired.tailnet.ts.net",
  credentialId: "paired-credential",
  label: "Paired",
  alias: "PAI",
};
let cleanups: (() => void)[];
function mount() {
  useUiPreferenceSync([local, { ...paired, workspaceId: "stale-workspace" }]);
  cleanups = mocks.effects.flatMap((effect) => {
    const cleanup = effect();
    return cleanup ? [cleanup] : [];
  });
}
beforeEach(() => {
  mocks.effects.length = 0;
  mocks.streams.length = 0;
  mocks.fetch.mockClear();
  cleanups = [];
  const values = new Map<string, string>();
  vi.stubGlobal("localStorage", {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => values.set(key, value),
    get length() {
      return values.size;
    },
    key: (index: number) => [...values.keys()][index] ?? null,
  });
  vi.stubGlobal("window", new EventTarget());
  localStorage.setItem(SERVER_REGISTRY_KEY, JSON.stringify([paired]));
});
afterEach(() => {
  cleanups.forEach((cleanup) => cleanup());
  vi.unstubAllGlobals();
});

it("leaves preference transport to mounted server projections", () => {
  mount();
  // Ordinary revision-only frames must have no independent preference listener.
  for (let revision = 1; revision <= 10; revision++) {
    for (const stream of mocks.streams) {
      stream.dispatchEvent(
        new MessageEvent("resources", {
          data: JSON.stringify({
            protocol: 4,
            workspaceId: "local-workspace",
            epoch: "epoch",
            reason: "change",
            resources: [{ kind: "state" }],
            resourceVersions: [{ revision }],
          }),
        }),
      );
    }
  }
  expect(mocks.streams).toHaveLength(0);
  expect(mocks.fetch).not.toHaveBeenCalled();
});

it("uses the projection workspace identity over a stale paired identity", () => {
  const key = JSON.stringify(["server-label:paired-workspace"]);
  localStorage.setItem("studio-server-workspace-id:paired", "paired-workspace");
  localStorage.setItem(
    `${preferenceCacheKey}:user`,
    JSON.stringify({
      [key]: { value: "Remote name", timestamp: 1, writer: "remote" },
    }),
  );
  mount();
  expect(JSON.parse(localStorage.getItem(SERVER_REGISTRY_KEY)!)[0].label).toBe(
    "Remote name",
  );
});

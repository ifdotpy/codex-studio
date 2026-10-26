import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  ModelCatalogReader,
  readWorkerModels,
  supportsDaybreakMode,
  useModelCatalogReader,
  type WorkerModelInfo,
} from "./WorkerModelPicker";

const boundary = vi.hoisted(() => ({
  browserImports: [] as string[],
  services: undefined as unknown,
}));

vi.mock("../../api", async () => {
  boundary.browserImports.push("api");
  return {
    ...(await import("../../apiErrors")),
    get: vi.fn(async () => ({ data: [] })),
    errorText: (error: unknown) => String(error),
  };
});
vi.mock("../../sync/client", () => {
  boundary.browserImports.push("sync/client");
  return { subscribeStateProjection: vi.fn(() => vi.fn()) };
});
vi.mock("../../servers/registry", () => {
  boundary.browserImports.push("servers/registry");
  return { serverResourceScope: () => "forbidden-local-fallback" };
});
vi.mock("../../servers/ServerServicesContext", () => {
  const required = () => {
    if (!boundary.services)
      throw new Error("The native screen requires its own server services.");
    return boundary.services as ReturnType<typeof services>;
  };
  return {
    useServerServices: () => boundary.services,
    useServerOwner: () => required().owner,
    useServerApi: () => required().api,
    useServerSync: () => required().sync,
  };
});

type Reader = ReturnType<typeof useModelCatalogReader>;
type Projection = Parameters<Parameters<Reader["subscribeState"]>[0]>[0];

function services(id: string, credentialId: string | null = "credential") {
  let accept: ((snapshot: Projection | null) => void) | undefined;
  const stop = vi.fn();
  return {
    owner: Object.freeze({
      id,
      origin: `https://${id}.example.test`,
      credentialId,
    }),
    api: { get: vi.fn(async () => ({ data: [] })) },
    sync: {
      subscribeStateProjection: vi.fn((next: typeof accept) => {
        accept = next;
        return stop;
      }),
    },
    emit(snapshot: Projection) {
      if (!accept) throw new Error("The captured catalog has no subscriber.");
      accept(snapshot);
    },
    stop,
  };
}

function renderReader(override?: Reader) {
  let reader!: Reader;
  function Probe() {
    reader = useModelCatalogReader();
    return null;
  }
  renderToStaticMarkup(
    override
      ? createElement(
          ModelCatalogReader.Provider,
          { value: override },
          createElement(Probe),
        )
      : createElement(Probe),
  );
  return reader;
}

beforeEach(() => {
  boundary.services = services("selected");
});

it("imports the picker without evaluating browser API, sync, or registry owners", () => {
  expect(boundary.browserImports).toEqual([]);
});

it("keeps catalog reads and projection subscriptions on their captured owner", async () => {
  const first = services("first");
  boundary.services = first;
  const reader = renderReader();
  const second = services("second", null);
  boundary.services = second;
  const replacement = renderReader();
  expect(reader.scope).toBe(
    JSON.stringify([first.owner.id, first.owner.origin, "credential"]),
  );
  expect(replacement.scope).toBe(
    JSON.stringify([second.owner.id, second.owner.origin, null]),
  );
  expect(reader.cacheOwner).toBe(first.sync);
  expect(replacement.cacheOwner).toBe(second.sync);
  await reader.read({ query: { account_key: "account" } });
  expect(first.api.get).toHaveBeenCalledExactlyOnceWith("/api/models", {
    query: { account_key: "account" },
  });
  expect(second.api.get).not.toHaveBeenCalled();
  const models = readWorkerModels(reader, "account");
  first.emit({
    entitiesComplete: true,
    runtime: {
      modelsByAccount: {
        account: [
          {
            model: "captured-model",
            provider: "codex",
            accountKey: "account",
            catalogPosition: 0,
          },
        ],
      },
    },
  });
  expect((await models).map((row) => row.model)).toEqual(["captured-model"]);
  expect(first.stop).toHaveBeenCalledOnce();
  expect(second.sync.subscribeStateProjection).not.toHaveBeenCalled();
});

it("preserves an explicit catalog reader instead of replacing its identity", () => {
  const override: Reader = {
    scope: "explicit-catalog",
    read: vi.fn(async () => ({ data: [] })),
    subscribeState: vi.fn(() => vi.fn()),
  };
  expect(renderReader(override)).toBe(override);
});

it("refuses missing injected services instead of acquiring a local catalog", () => {
  boundary.services = undefined;
  expect(() => renderReader()).toThrow("requires its own server services");
});

const modelInfo = (...args: [] | [unknown]): WorkerModelInfo => {
  const base = {
    model: "claude-sonnet",
    serviceTiers: [],
    supportedReasoningEfforts: [],
  };
  return args.length ? { ...base, availableAccessPrograms: args[0] } : base;
};

describe("supportsDaybreakMode", () => {
  it("keeps standard mode available when the cyber catalog field is absent", () => {
    expect(supportsDaybreakMode(modelInfo(), false)).toBe(true);
    expect(supportsDaybreakMode(modelInfo(null), false)).toBe(false);
  });

  it("requires explicit standard access when cyber metadata is present", () => {
    expect(
      supportsDaybreakMode(modelInfo({ cyber: ["standard"] }), false),
    ).toBe(true);
    expect(
      supportsDaybreakMode(modelInfo({ cyber: ["daybreakBlue"] }), false),
    ).toBe(false);
  });

  it.each([null, "standard", { unexpected: true }])(
    "does not grant standard mode for malformed cyber metadata: %s",
    (cyber) => {
      expect(supportsDaybreakMode(modelInfo({ cyber }), false)).toBe(false);
    },
  );

  it.each([
    ["standard", 1],
    ["daybreakBlue", 1],
  ])("rejects mixed-type cyber access entries: %s", (program, invalid) => {
    const info = modelInfo({ cyber: [program, invalid] });
    expect(supportsDaybreakMode(info, false)).toBe(false);
    expect(supportsDaybreakMode(info, true)).toBe(false);
  });

  it.each([null, "bad", []])(
    "does not grant standard mode for malformed access metadata: %s",
    (availableAccessPrograms) => {
      expect(
        supportsDaybreakMode(modelInfo(availableAccessPrograms), false),
      ).toBe(false);
    },
  );
});

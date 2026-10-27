import { required } from "../componentTestFixtures";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { INHERITED_MODEL, toModelOptions } from "./modelOption";
import {
  modelWhitelistKey,
  validModelWhitelist,
  readModelWhitelist,
  subscribeModelWhitelist,
  whitelistedModelOptions,
  writeModelWhitelist,
} from "./modelWhitelist";

const models = toModelOptions([
  {
    model: "default",
    resolvedModel: "claude-opus-4-6",
    provider: "claude",
    isDefault: true,
  },
  { model: "opus", resolvedModel: "claude-opus-4-6", provider: "claude" },
  { model: "gpt-6-astra", provider: "codex" },
]);
beforeEach(() => {
  const values = new Map<string, string>();
  vi.stubGlobal("localStorage", {
    get length() {
      return values.size;
    },
    key: (i: number) => [...values.keys()][i] ?? null,
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => {
      values.set(key, value);
    },
    removeItem: (key: string) => {
      values.delete(key);
    },
    clear: () => values.clear(),
  });
  vi.stubGlobal("window", new EventTarget());
  vi.stubGlobal("document", new EventTarget());
});
afterEach(() => {
  vi.unstubAllGlobals();
});
it("writes a versioned server preference instead of session memory", async () => {
  writeModelWhitelist([], "local");
  const { readPreferenceFields } = await import("../../sync/uiPreferenceStore");
  expect(
    readPreferenceFields("server")['["studio-model-whitelist-v1"]'],
  ).toMatchObject({
    value: [],
    timestamp: expect.any(Number),
    writer: expect.any(String),
  });
  vi.resetModules();
  const reloaded = await import("./modelWhitelist");
  expect(reloaded.readModelWhitelist("local")).toEqual([]);
});
describe("model whitelist", () => {
  it("defaults to all models, including future catalog entries", () => {
    expect(readModelWhitelist()).toBeNull();
    expect(whitelistedModelOptions(models, "gpt-6-astra", null)).toEqual(
      models,
    );
  });
  it("uses resolved provider-qualified identities for aliases", () => {
    const allowed = [modelWhitelistKey(required(models[1]))];
    expect(whitelistedModelOptions(models, "opus", allowed)).toEqual(
      models.slice(0, 2),
    );
    expect(modelWhitelistKey({ model: "opus", provider: "codex" })).not.toBe(
      modelWhitelistKey(required(models[1])),
    );
  });
  it("preserves an excluded current model as a disabled choice without changing catalog metadata", () => {
    const visible = whitelistedModelOptions(models, "gpt-6-astra", [
      modelWhitelistKey(required(models[1])),
    ]);
    expect(visible).toHaveLength(3);
    expect(required(visible[2])).toMatchObject({
      value: "gpt-6-astra",
      badges: ["current"],
      disabledReason: expect.stringContaining("Hidden in Studio settings"),
    });
    expect(required(models[2]).disabledReason).toBeUndefined();
  });
  it("keeps inherited settings available with no whitelisted models", () => {
    const inherited = {
      value: INHERITED_MODEL,
      label: "Same as caller",
      provider: "codex",
      badges: [],
    };
    expect(
      whitelistedModelOptions([inherited, ...models], INHERITED_MODEL, []),
    ).toEqual([inherited]);
  });
  it("notifies every mounted picker when settings change and unsubscribes", () => {
    let calls = 0;
    const unsubscribe = subscribeModelWhitelist(() => calls++);
    writeModelWhitelist(["codex:gpt-6-astra", "codex:gpt-6-astra"]);
    expect(readModelWhitelist()).toEqual(["codex:gpt-6-astra"]);
    expect(calls).toBe(1);
    unsubscribe();
    writeModelWhitelist(null);
    expect(calls).toBe(1);
  });
});

it("keeps catalog owners isolated even when their model IDs differ", () => {
  writeModelWhitelist(["codex:local-only"], "local");
  expect(readModelWhitelist("remote")).toBeNull();
  expect(
    whitelistedModelOptions(
      models,
      "gpt-6-astra",
      readModelWhitelist("remote"),
    ),
  ).toEqual(models);
  writeModelWhitelist([], "remote");
  expect(readModelWhitelist("local")).toEqual(["codex:local-only"]);
  writeModelWhitelist(null, "local");
  writeModelWhitelist(null, "remote");
});

it("does not retain an excluded default for a new draft", () => {
  expect(
    whitelistedModelOptions(
      models,
      "gpt-6-astra",
      [modelWhitelistKey(required(models[0]))],
      false,
    ),
  ).toEqual(models.slice(0, 2));
});

it("rejects malformed whitelist values before writing", () => {
  for (const value of [
    true,
    {},
    [1],
    [""],
    ["astra"],
    [":opus"],
    ["codex:"],
    ["codex:a", "codex:a"],
  ]) {
    expect(validModelWhitelist(value)).toBe(false);
  }
  expect(validModelWhitelist(null)).toBe(true);
  expect(validModelWhitelist([])).toBe(true);
  expect(
    validModelWhitelist(["codex:future-model", "claude:future-model"]),
  ).toBe(true);
});

import { required } from "../componentTestFixtures";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { preferenceMemory } from "../../servers/storage";
import { INHERITED_MODEL, toModelOptions } from "./modelOption";
import {
  isModelEnabled,
  modelChoiceKey,
  readModelChoices,
  subscribeModelChoices,
  enabledModelOptions,
  writeModelChoices,
} from "./modelChoices";

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
  preferenceMemory.clear();
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
  writeModelChoices({ "codex:gpt-6-astra": "blacklisted" }, "local");
  const { readPreferenceFields } = await import("../../sync/uiPreferenceStore");
  expect(
    readPreferenceFields("server")['["studio-model-choices-v1"]'],
  ).toMatchObject({
    value: { "codex:gpt-6-astra": "blacklisted" },
    timestamp: expect.any(Number),
    writer: expect.any(String),
  });
  // Preferences live in memory and return from the server stream, not from localStorage.
  expect(localStorage.length).toBe(0);
  expect(readModelChoices("local")).toEqual({
    "codex:gpt-6-astra": "blacklisted",
  });
});
describe("model choices", () => {
  it("enables untouched and whitelisted models, including future catalog entries", () => {
    expect(readModelChoices()).toEqual({});
    expect(enabledModelOptions(models, "gpt-6-astra", {})).toEqual(models);
    const opus = modelChoiceKey(required(models[1]));
    expect(isModelEnabled(required(models[1]), { [opus]: "whitelisted" })).toBe(
      true,
    );
    expect(isModelEnabled(required(models[1]), { [opus]: "untouched" })).toBe(
      true,
    );
    expect(isModelEnabled(required(models[1]), { [opus]: "blacklisted" })).toBe(
      false,
    );
    expect(
      enabledModelOptions(models, "opus", { [opus]: "whitelisted" }),
    ).toEqual(models);
  });
  it("uses resolved provider-qualified identities for aliases", () => {
    const blacklisted = {
      [modelChoiceKey(required(models[2]))]: "blacklisted",
    } as const;
    expect(enabledModelOptions(models, "opus", blacklisted)).toEqual(
      models.slice(0, 2),
    );
    expect(modelChoiceKey({ model: "opus", provider: "codex" })).not.toBe(
      modelChoiceKey(required(models[1])),
    );
  });
  it("preserves a blacklisted current model as a disabled choice without changing catalog metadata", () => {
    const visible = enabledModelOptions(models, "gpt-6-astra", {
      "codex:gpt-6-astra": "blacklisted",
    });
    expect(visible).toHaveLength(3);
    expect(required(visible[2])).toMatchObject({
      value: "gpt-6-astra",
      badges: ["current"],
      disabledReason: expect.stringContaining("Hidden in Studio settings"),
    });
    expect(required(models[2]).disabledReason).toBeUndefined();
  });
  it("keeps inherited settings available with every model blacklisted", () => {
    const inherited = {
      value: INHERITED_MODEL,
      label: "Same as caller",
      provider: "codex",
      badges: [],
    };
    expect(
      enabledModelOptions([inherited, ...models], INHERITED_MODEL, {
        "claude:claude-opus-4-6": "blacklisted",
        "codex:gpt-6-astra": "blacklisted",
      }),
    ).toEqual([inherited]);
  });
  it("notifies every mounted picker when settings change and unsubscribes", () => {
    let calls = 0;
    const unsubscribe = subscribeModelChoices(() => calls++);
    writeModelChoices({ "codex:gpt-6-astra": "whitelisted" });
    expect(readModelChoices()).toEqual({ "codex:gpt-6-astra": "whitelisted" });
    expect(calls).toBe(1);
    unsubscribe();
    writeModelChoices({});
    expect(calls).toBe(1);
  });
});

it("keeps catalog owners isolated even when their model IDs differ", () => {
  writeModelChoices({ "codex:local-only": "blacklisted" }, "local");
  expect(readModelChoices("remote")).toEqual({});
  writeModelChoices({ "codex:gpt-6-astra": "blacklisted" }, "remote");
  expect(readModelChoices("local")).toEqual({
    "codex:local-only": "blacklisted",
  });
  writeModelChoices({}, "local");
  writeModelChoices({}, "remote");
});

it("does not retain a blacklisted default for a new draft", () => {
  expect(
    enabledModelOptions(
      models,
      "gpt-6-astra",
      { "codex:gpt-6-astra": "blacklisted" },
      false,
    ),
  ).toEqual(models.slice(0, 2));
});

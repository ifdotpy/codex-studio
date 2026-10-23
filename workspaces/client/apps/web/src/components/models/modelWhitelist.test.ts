import { afterEach, describe, expect, it } from "vitest";
import { INHERITED_MODEL, toModelOptions } from "./modelOption";
import {
  modelWhitelistKey,
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
afterEach(() => writeModelWhitelist(null));
describe("model whitelist", () => {
  it("defaults to all models, including future catalog entries", () => {
    expect(readModelWhitelist()).toBeNull();
    expect(whitelistedModelOptions(models, "gpt-6-astra", null)).toEqual(
      models,
    );
  });
  it("uses resolved provider-qualified identities for aliases", () => {
    const allowed = [modelWhitelistKey(models[1])];
    expect(whitelistedModelOptions(models, "opus", allowed)).toEqual(
      models.slice(0, 2),
    );
    expect(modelWhitelistKey({ model: "opus", provider: "codex" })).not.toBe(
      modelWhitelistKey(models[1]),
    );
  });
  it("preserves an excluded current model as a disabled choice without changing catalog metadata", () => {
    const visible = whitelistedModelOptions(models, "gpt-6-astra", [
      modelWhitelistKey(models[1]),
    ]);
    expect(visible).toHaveLength(3);
    expect(visible[2]).toMatchObject({
      value: "gpt-6-astra",
      badges: ["current"],
      disabledReason: expect.stringContaining("Hidden in Studio settings"),
    });
    expect(models[2].disabledReason).toBeUndefined();
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

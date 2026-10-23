import { describe, expect, it } from "vitest";
import {
  INHERITED_MODEL,
  modelDisplayName,
  modelOptionLabel,
  toModelOptions,
} from "./modelOption";

describe("modelDisplayName", () => {
  it.each([
    ["gpt-6-astra", "Astra 6"],
    ["gpt-6.1-sol", "Sol 6.1"],
    ["gpt-5.6-sol", "Sol 5.6"],
    ["gpt-daybreak-blue-latest", "Daybreak Blue"],
    ["claude-opus-4-6", "Opus 4.6"],
    ["unknown-model", "unknown-model"],
  ])("names %s consistently", (model, label) =>
    expect(modelDisplayName(model)).toBe(label),
  );
  it("reads Claude alias versions from descriptions and preserves labels", () => {
    expect(
      modelDisplayName({
        model: "sonnet",
        provider: "claude",
        displayName: "Claude · Sonnet",
        description: "Claude Sonnet 4.6 · Balanced",
      }),
    ).toBe("Sonnet 4.6");
    expect(
      modelDisplayName({ model: "custom", displayName: "Custom model" }),
    ).toBe("Custom model");
    expect(
      modelDisplayName({
        model: "claude-opus-4-6",
        provider: "claude",
        description: "Claude Opus 4.6 · Reasoning",
      }),
    ).toBe("Opus 4.6");
    expect(
      modelDisplayName({
        model: "default",
        isDefault: true,
        resolvedModel: "claude-opus-5-5",
        displayName: "Claude · Default (recommended)",
      }),
    ).toBe("Opus 5.5");
    expect(modelDisplayName(undefined, "gpt-6-astra")).toBe("Astra 6");
  });
});

describe("Claude description aliases", () => {
  it.each([
    [
      "Claude · Default (recommended)",
      "Opus 4.6 · Best for reasoning",
      "Opus 4.6",
    ],
    ["Claude · Sonnet", "Claude Sonnet 4.6 · Balanced", "Sonnet 4.6"],
    ["Claude · Custom", "Opus 4.6 · Custom deployment", "Custom · Opus 4.6"],
  ])(
    "keeps the description family for %s",
    (displayName, description, expected) => {
      expect(
        modelDisplayName({ model: "default", displayName, description }),
      ).toBe(expected);
    },
  );
});

describe("toModelOptions", () => {
  it("preserves values, provider, descriptions, default/current tags and disabled reasons", () => {
    const options = toModelOptions(
      [
        { model: "gpt-6-astra", isDefault: true, description: "General work" },
        { model: "claude-opus-4-6", provider: "claude" },
      ],
      {
        currentValue: "gpt-6-astra",
        disabledReason: (row) =>
          row.provider === "claude" ? "Connect a Claude account." : undefined,
      },
    );
    expect(options[0]).toEqual({
      value: "gpt-6-astra",
      label: "Astra 6",
      provider: "codex",
      description: "General work",
      badges: ["default", "current"],
      disabledReason: undefined,
    });
    expect(options[1].disabledReason).toBe("Connect a Claude account.");
    expect(modelOptionLabel(options[0])).toBe("Astra 6 (default, current)");
    expect(modelOptionLabel(options[1])).toBe("Opus 4.6");
    expect(INHERITED_MODEL).toBe("__model_default__");
  });
});

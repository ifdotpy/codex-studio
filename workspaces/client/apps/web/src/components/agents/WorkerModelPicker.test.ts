import { describe, expect, it } from "vitest";
import {
  supportsDaybreakMode,
  type WorkerModelInfo,
} from "./WorkerModelPicker";

const modelInfo = (
  ...args: [] | [WorkerModelInfo["availableAccessPrograms"]]
): WorkerModelInfo => {
  const base = {
    model: "claude-sonnet",
    serviceTiers: [],
    supportedReasoningEfforts: [],
  } satisfies WorkerModelInfo;
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

  it("consumes the generated catalog contract", () => {
    const valid = {
      model: "future-model",
      supportedReasoningEfforts: [{ reasoningEffort: "future-effort" }],
      serviceTiers: [],
    } satisfies WorkerModelInfo;
    expect(valid.supportedReasoningEfforts[0]?.reasoningEffort).toBe(
      "future-effort",
    );
    // @ts-expect-error Known catalog booleans must not widen to opaque metadata.
    const invalidHidden: WorkerModelInfo = { ...valid, hidden: "yes" };
    const invalidEffort: WorkerModelInfo = {
      ...valid,
      // @ts-expect-error Upstream-owned effort names stay strings, not untyped JSON.
      supportedReasoningEfforts: [{ reasoningEffort: 1 }],
    };
    expect(invalidHidden.hidden).toBe("yes");
    expect(invalidEffort.supportedReasoningEfforts[0]?.reasoningEffort).toBe(1);
  });
});

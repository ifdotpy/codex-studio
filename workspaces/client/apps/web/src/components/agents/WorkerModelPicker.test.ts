import { describe, expect, it, vi } from "vitest";
import {
  readWorkerModels,
  supportsDaybreakMode,
  type WorkerModelInfo,
} from "./WorkerModelPicker";

it.each([false, true])(
  "account selection requires fresh metadata with automatic=%s",
  async (workers) => {
    const read = vi.fn(async () => ({
      data: [{ model: "gpt-6.1-sol", provider: "codex" as const }],
    }));
    const rows = await readWorkerModels(
      {
        scope: "account-choice",
        read,
        subscribeState: vi.fn(),
      },
      "selected-account",
      workers,
    );
    expect(read).toHaveBeenCalledExactlyOnceWith({
      query: {
        account_key: "selected-account",
        workers: workers ? "1" : undefined,
        fresh: "1",
      },
    });
    expect(rows.map((row) => row.model)).toEqual(["gpt-6.1-sol"]);
  },
);

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
    const invalidProvider: WorkerModelInfo = {
      ...valid,
      // @ts-expect-error Catalog provider identity comes from ProviderKind.
      provider: "future-provider",
    };
    const invalidEffort: WorkerModelInfo = {
      ...valid,
      // @ts-expect-error Upstream-owned effort names stay strings, not untyped JSON.
      supportedReasoningEfforts: [{ reasoningEffort: 1 }],
    };
    expect(invalidHidden.hidden).toBe("yes");
    expect(invalidProvider.provider).toBe("future-provider");
    expect(invalidEffort.supportedReasoningEfforts[0]?.reasoningEffort).toBe(1);
  });
});

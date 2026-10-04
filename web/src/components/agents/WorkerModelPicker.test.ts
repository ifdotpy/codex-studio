import { describe, expect, it } from "vitest";
import {
  consumeModelRetryTicket,
  supportsDaybreakMode,
  type WorkerModelInfo,
} from "./WorkerModelPicker";

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

describe("consumeModelRetryTicket", () => {
  it("consumes a matching explicit retry once and never for SSE reloads", () => {
    const ticket = { current: { key: "account:false", attempt: 3 } };

    expect(consumeModelRetryTicket(ticket, "account:false", 2)).toBe(false);
    expect(ticket.current).toEqual({ key: "account:false", attempt: 3 });
    expect(consumeModelRetryTicket(ticket, "account:false", 3)).toBe(true);
    expect(consumeModelRetryTicket(ticket, "account:false", 3)).toBe(false);
  });

  it("discards a retry ticket if the selected catalog changed", () => {
    const ticket = { current: { key: "account:false", attempt: 3 } };

    expect(consumeModelRetryTicket(ticket, "other:true", 3)).toBe(false);
    expect(ticket.current).toBeNull();
  });
});

import { describe, expect, it } from "vitest";
import type { Json } from "../../types";
import { parseRuleDraft } from "./Workspace";

describe("parseRuleDraft", () => {
  it("restores persisted form values with recognized API enums", () => {
    const savedDraft: Json = {
      id: "rule-1",
      isNew: true,
      name: "Nightly review",
      kind: "once",
      intervalSeconds: 60,
      minimumWorkers: 8,
      durationMinutes: 30,
      at: "2026-10-05T09:00",
      path: "",
      event: "worker_completed",
      command: "",
      text: "Review pending work",
    };

    expect(parseRuleDraft(savedDraft)).toEqual(savedDraft);
  });

  it("rejects unknown rule kinds and safely defaults malformed optional values", () => {
    expect(
      parseRuleDraft({ id: "rule-1", name: "bad", kind: "unknown" }),
    ).toBeNull();
    expect(
      parseRuleDraft({ id: "rule-2", name: "valid", kind: "interval" }),
    ).toMatchObject({
      id: "rule-2",
      kind: "interval",
      intervalSeconds: 60,
      minimumWorkers: 8,
      durationMinutes: 30,
      event: "worker_completed",
    });
  });
});

import { describe, expect, it } from "vitest";
import type { Json } from "../../types";
import {
  parseRuleDraft,
  ruleBodyFromDraft,
  ruleDraftFromRecord,
} from "./Workspace";

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
      stallTimeoutSeconds: 1800,
      at: "2026-10-05T09:00",
      path: "",
      event: "worker_completed",
      command: "",
      livenessCommand: "",
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

it("preserves file-watch timeout and liveness command across edit and save", () => {
  const draft = ruleDraftFromRecord({
    id: "rule-file",
    name: "Watch generated files",
    kind: "file",
    path: "dist",
    stallTimeoutSeconds: 73,
    livenessCommand: "test -f ready.flag",
    command: "npm run check-files",
    text: "Files changed",
  });
  const savedDraft: Json = { ...draft };
  const restored = parseRuleDraft(savedDraft);
  if (!restored) throw new Error("Expected the persisted rule draft to parse");

  expect(restored.stallTimeoutSeconds).toBe(73);
  expect(restored.livenessCommand).toBe("test -f ready.flag");
  expect(ruleBodyFromDraft(restored, "lead-1")).toMatchObject({
    stallTimeoutSeconds: 73,
    livenessCommand: "test -f ready.flag",
    command: "npm run check-files",
  });
});

import { describe, expect, test } from "vitest";
import contract from "../../../../contracts/account-preference.json";
import {
  defaultChatModel,
  newChatDefault,
  parseGroups,
  type PreferenceAccount,
  type PreferenceProject,
} from "./accountPreference";

type Case = {
  name: string;
  accounts: PreferenceAccount[];
  defaultAccountKey: string;
  defaultAccountGroups: string[];
  project: PreferenceProject | null;
  model: string | null;
  expected: { accountKey: string; model: string | null; effort: string | null };
};

describe("the shared account preference cases", () => {
  const cases = contract.cases as Case[];
  test("cover the rules", () => expect(cases.length).toBeGreaterThan(10));
  test.each(cases.map((item) => [item.name, item] as const))(
    "%s",
    (_name, item) =>
      expect(
        newChatDefault(
          item.accounts,
          item.defaultAccountKey,
          item.defaultAccountGroups,
          item.project,
          item.model,
        ),
      ).toEqual(item.expected),
  );
});

describe("the new chat model", () => {
  const claudeCatalog = [
    { model: "default", resolvedModel: "claude-sonnet-5-5", isDefault: true },
    { model: "opus", resolvedModel: "claude-opus-5-5" },
  ];
  test("a Claude orchestrator starts on Opus 5.5 when the catalog has it", () =>
    expect(defaultChatModel("claude", claudeCatalog)).toBe("claude-opus-5-5"));
  test("a Claude catalog without Opus 5.5 keeps its default model", () =>
    expect(defaultChatModel("claude", claudeCatalog.slice(0, 1))).toBe(
      "default",
    ));
  test("a Codex orchestrator keeps Astra", () =>
    expect(
      defaultChatModel("codex", [
        { model: "gpt-6-luna", isDefault: true },
        { model: "gpt-6-astra" },
      ]),
    ).toBe("gpt-6-astra"));
});

test("an account order is typed as group names", () => {
  expect(parseGroups(" Attar, personal  lumina,attar ")).toEqual({
    groups: ["attar", "personal", "lumina"],
    error: "",
  });
  expect(parseGroups("")).toEqual({ groups: [], error: "" });
  expect(parseGroups("ok, -bad").error).toContain('"-bad"');
  expect(
    parseGroups(Array.from({ length: 17 }, (_, i) => `g${i}`).join(",")).error,
  ).toContain("at most 16");
});

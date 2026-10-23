import { readFileSync } from "node:fs";
import { expect, test } from "vitest";
import {
  capabilityDraft,
  capabilityLists,
  parseCapabilities,
  profileCapabilityRules,
} from "./profileCapabilityFields";

test("omitted and null values inherit; empty arrays survive an API round trip", () => {
  const inherited = {
    allowedTools: null,
    disallowedTools: null,
    mcpServers: null,
    skills: null,
    maxTurns: null,
  };
  expect(parseCapabilities(capabilityDraft({})).values).toEqual(inherited);
  expect(parseCapabilities(capabilityDraft(inherited)).values).toEqual(
    inherited,
  );
  const explicit = {
    allowedTools: [],
    disallowedTools: [],
    mcpServers: [],
    skills: [],
    maxTurns: 1,
  };
  const saved = parseCapabilities(capabilityDraft(explicit));
  expect(saved.values).toEqual(explicit);
  expect(
    parseCapabilities(capabilityDraft(JSON.parse(JSON.stringify(saved.values))))
      .values,
  ).toEqual(explicit);
});

test("persisted text drafts retain all five fields and invalid input", () => {
  const draft = capabilityDraft({
    allowedTools: "Read\n mcp__docs__* ",
    disallowedTools: "Bash",
    mcpServers: "docs/api:v1.0-beta_2",
    skills: "review/code",
    maxTurns: "42",
  });
  expect(parseCapabilities(draft)).toEqual({
    values: {
      allowedTools: ["Read", "mcp__docs__*"],
      disallowedTools: ["Bash"],
      mcpServers: ["docs/api:v1.0-beta_2"],
      skills: ["review/code"],
      maxTurns: 42,
    },
    errors: {},
  });
  expect(capabilityDraft({ maxTurns: "1.5" }).maxTurns).toBe("1.5");
});

test.each(capabilityLists)(
  "$field validates names and explicit empty text",
  ({ field, patterns }) => {
    for (const invalid of [
      "../outside",
      "a/../b",
      "..",
      "café",
      "two names",
      "name?",
      "name,other",
      "Read(file)",
    ]) {
      const result = parseCapabilities(capabilityDraft({ [field]: invalid }));
      expect(result.values, invalid).toBeNull();
      expect(result.errors[field], invalid).toContain("invalid name");
    }
    const star = parseCapabilities(
      capabilityDraft({ [field]: "mcp__docs__*" }),
    );
    expect(star.values !== null).toBe(patterns);
    expect(
      parseCapabilities(capabilityDraft({ [field]: "a..b/x" })).values?.[field],
    ).toEqual(["a..b/x"]);
    expect(
      parseCapabilities(capabilityDraft({ [field]: " \n\r\n " })).values?.[
        field
      ],
    ).toEqual([]);
    expect(
      parseCapabilities(capabilityDraft({ [field]: null })).values?.[field],
    ).toBeNull();
  },
);

test.each([
  "0",
  "-1",
  "1.5",
  "1e3",
  "NaN",
  "Infinity",
  "4294967296",
  "9007199254740992",
  "turns",
])("rejects invalid maxTurns %s without producing a save body", (maxTurns) => {
  const result = parseCapabilities(capabilityDraft({ maxTurns }));
  expect(result.values).toBeNull();
  expect(result.errors.maxTurns).toContain("whole number from 1 to 4294967295");
});

test.each([1, 4294967295])("preserves maxTurns boundary %i", (maxTurns) => {
  expect(
    parseCapabilities(capabilityDraft({ maxTurns })).values?.maxTurns,
  ).toBe(maxTurns);
});

test.each([null, undefined, "", " "])(
  "blank maxTurns %s inherits",
  (maxTurns) => {
    expect(
      parseCapabilities(capabilityDraft({ maxTurns })).values?.maxTurns,
    ).toBeNull();
  },
);

test("UI validation matches the worker_capabilities.rs owner schema", () => {
  const schema = JSON.parse(
    readFileSync(
      new URL(
        "../../../../../../runtime/packages/runtime-schema/src/schemas/persisted-profile-record.schema.json",
        import.meta.url,
      ),
      "utf8",
    ),
  );
  const turns = schema.properties.maxTurns.anyOf.find(
    (item: { type: string }) => item.type === "integer",
  );
  expect([
    profileCapabilityRules.minTurns,
    profileCapabilityRules.maxTurns,
  ]).toEqual([turns.minimum, turns.maximum]);
  for (const { field, patterns } of capabilityLists) {
    const array = schema.properties[field].anyOf.find(
      (item: { type: string }) => item.type === "array",
    );
    expect(
      patterns
        ? profileCapabilityRules.toolPattern
        : profileCapabilityRules.namePattern,
      field,
    ).toBe(array.items.pattern);
  }
});

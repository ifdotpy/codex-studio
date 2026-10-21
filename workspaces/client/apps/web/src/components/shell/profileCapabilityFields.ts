import type { PostBody } from "../../api";

export const capabilityLists = [
  {
    field: "allowedTools",
    label: "Allowed tools",
    empty: "No allowed tools.",
    patterns: true,
  },
  {
    field: "disallowedTools",
    label: "Disallowed tools",
    empty: "No additional denied tools.",
    patterns: true,
  },
  {
    field: "mcpServers",
    label: "MCP servers",
    empty: "No user MCP servers.",
    patterns: false,
  },
  {
    field: "skills",
    label: "Skills",
    empty: "No skill preload.",
    patterns: false,
  },
] as const;

export type CapabilityListField = (typeof capabilityLists)[number]["field"];
export type CapabilityField = CapabilityListField | "maxTurns";
export type CapabilityDraft = Record<CapabilityField, string | null>;
export type CapabilityErrors = Partial<Record<CapabilityField, string>>;
export type ProfileCapabilityValues = Required<
  Pick<PostBody<"/api/profiles">, CapabilityField>
>;

// Owner: runtime-schema/src/worker_capabilities.rs. The adjacent test checks
// these UI bounds and patterns against its generated persistence schema.
export const profileCapabilityRules = {
  minTurns: 1,
  maxTurns: 4294967295,
  toolPattern: "^[A-Za-z0-9_.:/*-]+$",
  namePattern: "^[A-Za-z0-9_.:/-]+$",
} as const;

/** Read both saved API arrays and editable text from the persistent form draft. */
export function capabilityDraft(
  source: Partial<Record<CapabilityField, unknown>>,
): CapabilityDraft {
  const listText = (field: CapabilityListField) => {
    const value = source[field];
    if (value === null || value === undefined) return null;
    return Array.isArray(value) ? value.join("\n") : String(value);
  };
  return {
    allowedTools: listText("allowedTools"),
    disallowedTools: listText("disallowedTools"),
    mcpServers: listText("mcpServers"),
    skills: listText("skills"),
    maxTurns: source.maxTurns == null ? null : String(source.maxTurns),
  };
}

export function parseCapabilities(
  draft: CapabilityDraft,
):
  | { values: ProfileCapabilityValues; errors: CapabilityErrors }
  | { values: null; errors: CapabilityErrors } {
  const errors: CapabilityErrors = {};
  const values: ProfileCapabilityValues = {
    allowedTools: null,
    disallowedTools: null,
    mcpServers: null,
    skills: null,
    maxTurns: null,
  };
  for (const { field, label, patterns } of capabilityLists) {
    const text = draft[field];
    if (text === null) continue;
    const names = text
      .split(/\r?\n/)
      .map((name) => name.trim())
      .filter(Boolean);
    const pattern = new RegExp(
      patterns
        ? profileCapabilityRules.toolPattern
        : profileCapabilityRules.namePattern,
    );
    const invalid = names.find(
      (name) => !pattern.test(name) || name.split("/").includes(".."),
    );
    if (invalid !== undefined) {
      errors[field] =
        `${label}: invalid name "${invalid}". Use ASCII letters, digits, _-.:/${patterns ? " or *" : ""}. Do not use a '..' path segment.`;
    }
    values[field] = names;
  }
  const turns = draft.maxTurns?.trim() ?? "";
  if (turns !== "") {
    const value = Number(turns);
    if (
      !/^\d+$/.test(turns) ||
      !Number.isSafeInteger(value) ||
      value < profileCapabilityRules.minTurns ||
      value > profileCapabilityRules.maxTurns
    ) {
      errors.maxTurns = `Maximum turns must be a whole number from ${profileCapabilityRules.minTurns} to ${profileCapabilityRules.maxTurns}.`;
    } else {
      values.maxTurns = value;
    }
  }
  return { values: Object.keys(errors).length ? null : values, errors };
}

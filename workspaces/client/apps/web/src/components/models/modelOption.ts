/** Inherited execution model sentinel; the server receives null for this choice. */
export const INHERITED_MODEL = "__model_default__";

export type ModelOption = {
  value: string;
  resolvedModel?: string;
  label: string;
  provider: string;
  description?: string | undefined;
  badges: string[];
  disabledReason?: string | undefined;
};

type CatalogModel = {
  model: string;
  displayName?: string;
  description?: string;
  provider?: string;
  isDefault?: boolean;
  resolvedModel?: string;
};

/** Match stored IDs to catalog or option values; explicit aliases beat default aliases. */
export function modelForValue<
  T extends {
    model?: string;
    value?: string;
    resolvedModel?: string;
    isDefault?: boolean;
    badges?: string[];
  },
>(rows: T[], value?: string | null): T | undefined {
  if (!value) return undefined;
  return (
    rows.find((row) => (row.model ?? row.value) === value) ??
    rows.find(
      (row) =>
        row.resolvedModel === value &&
        (row.model ?? row.value) !== "default" &&
        !row.isDefault &&
        !row.badges?.includes("default"),
    ) ??
    rows.find((row) => row.resolvedModel === value)
  );
}

/** Shared model name for setup, compact selection, summaries and worker cards. */
export function modelDisplayName(
  info?: CatalogModel | string,
  fallback = "",
): string {
  const row = typeof info === "string" ? { model: info } : info;
  const id = (row?.isDefault && row.resolvedModel) || row?.model || fallback;
  const versioned = /^gpt-(\d[\d.]*)-([a-z][a-z-]*)$/i.exec(id);
  if (versioned) {
    const [, number, family] = versioned;
    if (family === undefined) throw new Error("Missing model family");
    return `${family.split("-").map(title).join(" ")} ${number}`;
  }
  const daybreak = /^gpt-daybreak-(blue|red)-latest$/.exec(id);
  if (daybreak) {
    const [, family] = daybreak;
    if (family === undefined) throw new Error("Missing model family");
    return `Daybreak ${title(family)}`;
  }
  let label = (row?.displayName || id)
    .replace(/^Claude\s*[·: ]\s*/i, "")
    .replace(/^Default \(recommended\)\s*[:·]?\s*/i, "")
    .replace(/^GPT[-\s]+/i, "");
  const claudeId = /^claude-([a-z]+)-(\d+)-(\d+)$/.exec(label);
  if (claudeId) {
    const [, family, major, minor] = claudeId;
    if (family === undefined) throw new Error("Missing model family");
    label = `${title(family)} ${major}.${minor}`;
  }
  // Claude aliases often carry the family/version only in their description.
  const version = /^(?:Claude )?([A-Za-z]+) (\d+(?:\.\d+)*)(?=\s|$|[·(])/.exec(
    row?.description || "",
  );
  if (
    version &&
    /claude/i.test(row?.provider || id + (row?.displayName || ""))
  ) {
    const [, family, number] = version;
    const familyLabel = new RegExp(
      `\\b${family}(?: \\d+(?:\\.\\d+)*)?\\b`,
      "i",
    );
    label = familyLabel.test(label)
      ? label.replace(familyLabel, `${family} ${number}`)
      : [label, `${family} ${number}`].filter(Boolean).join(" · ");
  }
  return (
    label || (row?.resolvedModel ? modelDisplayName(row.resolvedModel) : id)
  );
}

const title = (word: string) => {
  const first = word[0];
  if (first === undefined) throw new Error("Missing model name word");
  return first.toUpperCase() + word.slice(1);
};

export function toModelOptions<T extends CatalogModel>(
  models: T[],
  {
    provider = "codex",
    currentValue,
    disabledReason,
  }: {
    provider?: string;
    currentValue?: string | null;
    disabledReason?: (model: T) => string | undefined;
  } = {},
): ModelOption[] {
  const selected = modelForValue(models, currentValue);
  return models.map((row) => ({
    value: row.model,
    ...(row.resolvedModel ? { resolvedModel: row.resolvedModel } : {}),
    label: modelDisplayName(row),
    provider: row.provider || provider,
    description: row.description || undefined,
    badges: [
      ...(row.isDefault ? ["default"] : []),
      ...(row === selected ? ["current"] : []),
    ],
    disabledReason: disabledReason?.(row),
  }));
}

export function modelOptionLabel(option: ModelOption): string {
  return option.badges.length
    ? `${option.label} (${option.badges.join(", ")})`
    : option.label;
}

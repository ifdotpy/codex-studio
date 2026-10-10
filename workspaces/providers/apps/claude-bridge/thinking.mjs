// Resolve aliases from the current account catalog, not a fixed alias mapping.
export function requiresThinking(model, models = []) {
  const row = models.find(
    (row) => (row.value || row.model) === model || row.resolvedModel === model,
  );
  const identity = row?.resolvedModel || model;
  const explicit = /^claude-(opus|fable)-(\d+)(?:-(\d+))?(?:\[1m\])?$/.exec(
    identity,
  );
  const described =
    /^(?:Claude )?(Opus|Fable) (\d+)(?:\.(\d+))?(?=\s|$|[·(])/i.exec(
      row?.description || "",
    );
  const match = explicit || described;
  if (!match) return false;
  const [, family, major, minor = "0"] = match;
  return (
    (family.toLowerCase() === "opus" && major === "5" && minor === "5") ||
    (family.toLowerCase() === "fable" &&
      major === "5" &&
      ["0", "1"].includes(minor))
  );
}

export function thinkingFlag(model, models, saved, live = false) {
  return requiresThinking(model, models)
    ? { alwaysThinkingEnabled: true }
    : saved === undefined
      ? live
        ? { alwaysThinkingEnabled: null }
        : {}
      : { alwaysThinkingEnabled: saved };
}

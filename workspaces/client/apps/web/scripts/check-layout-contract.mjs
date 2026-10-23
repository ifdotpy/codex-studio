import { readdirSync, readFileSync } from "node:fs";
import { resolve, relative } from "node:path";
import { fileURLToPath } from "node:url";
import {
  webRoot,
  readLayoutContract,
  validateContract,
  generateLayoutContract,
} from "./generate-layout-contract.mjs";
const normalize = (text) => text.replace(/\s+/g, " ").trim();
const annotation = (source, index, kind) => {
  const prefix = source.slice(Math.max(0, index - 150), index);
  return prefix.match(
    new RegExp(
      `/\\*\\s*layout-${kind}:\\s*([\\w.]+)(?:\\s+([<>=]+))?\\s*\\*/\\s*(?:[^{}]*\\{\\s*)?$`,
    ),
  );
};
export function scanSource(path, source, contract = readLayoutContract()) {
  const entries = [];
  const add = (kind, match, key, operator) =>
    entries.push({
      path,
      kind,
      signature: normalize(match[0]),
      key,
      operator,
      index: match.index,
    });
  if (path.endsWith(".css")) {
    for (const match of source.matchAll(/@(media|container)\s+([^{}]+)\{/g)) {
      if (!/\bwidth\b/.test(match[2])) continue;
      const note = annotation(source, match.index, "query");
      add("query", match, note?.[1], note?.[2]);
    }
  } else {
    for (const match of source.matchAll(
      /(?:matchMedia|useMediaQuery)\(\s*(?:(["'`])([^"'`]+)\1|([\w.]+))/g,
    )) {
      if (match[2] && !/\bwidth\b/.test(match[2])) continue;
      add(
        "query",
        match,
        match[3]?.startsWith("layoutQueries.")
          ? `query.${match[3].slice(14)}`
          : undefined,
      );
    }
  }
  for (const match of source.matchAll(
    /(?:z-index\s*:|zIndex\s*[:=]\s*\{?)\s*(-?\d+)/g,
  )) {
    const note = annotation(source, match.index, "layer");
    add("layer", match, note?.[1]);
  }
  if (path.endsWith(".css")) {
    const dimensions = new Set(
      Object.values(contract.tokens)
        .filter((t) => ["px", "rem"].includes(t.unit))
        .map((t) => t.value * (t.unit === "rem" ? 16 : 1)),
    );
    for (const match of source.matchAll(
      /(?:^|[;{\n])\s*((?:min-|max-)?(?:width|height)|flex-basis|--(?:studio-sidebar-width|chat-width|chat-text-width|studio-chat-max))\s*:\s*(\d+(?:\.\d+)?)px\b/g,
    )) {
      if (!dimensions.has(Number(match[2]))) continue;
      add("dimension", match);
    }
  }
  return entries;
}
export function sourceFiles(root = webRoot) {
  const result = new Map();
  function walk(directory) {
    for (const file of readdirSync(directory, { withFileTypes: true })) {
      const path = resolve(directory, file.name);
      if (file.isDirectory()) walk(path);
      else if (
        /\.(?:css|ts|tsx)$/.test(file.name) &&
        !/\.(?:test|spec|generated)\./.test(file.name)
      )
        result.set(relative(root, path), readFileSync(path, "utf8"));
    }
  }
  walk(resolve(root, "src"));
  return result;
}
export function checkConsumers(
  contract,
  files,
  { requireAllLegacy = true } = {},
) {
  validateContract(contract);
  const errors = [];
  const counts = new Map();
  const identity = (entry) =>
    JSON.stringify([entry.path, entry.kind, entry.signature]);
  const legacy = new Map();
  for (const entry of contract.legacyConsumers) {
    const id = identity(entry);
    if (legacy.has(id) || !entry.purpose || !entry.plannedOwner || !entry.key)
      errors.push(
        `duplicate or invalid owner ${entry.key} consumer ${entry.path}`,
      );
    legacy.set(id, entry);
  }
  for (const [path, source] of files)
    for (const entry of scanSource(path, source, contract)) {
      if (entry.key?.startsWith("query.")) {
        if (!contract.queries[entry.key.slice(6)])
          errors.push(`unknown owner ${entry.key} consumer ${path}`);
        continue;
      }
      if (entry.key) {
        const token = contract.tokens[entry.key];
        const numeric =
          entry.kind === "layer"
            ? Number(entry.signature.match(/(-?\d+)$/)?.[1])
            : Number(entry.signature.match(/(\d+(?:\.\d+)?)px/)?.[1]);
        const operator = entry.signature.match(/\bwidth\s*([<>=]+)/)?.[1];
        if (
          !token ||
          numeric !== token.value ||
          (entry.kind === "query" &&
            (token.unit !== "px" ||
              (entry.operator && operator !== entry.operator) ||
              !Object.values(contract.queries).some(
                (query) =>
                  query.key === entry.key && query.operator === operator,
              ) ||
              (entry.signature.match(/\bwidth\b/g) ?? []).length !== 1))
        )
          errors.push(
            `owner ${entry.key} differs at consumer ${path}: ${entry.signature}`,
          );
        continue;
      }
      const id = identity(entry);
      counts.set(id, (counts.get(id) ?? 0) + 1);
      if (!legacy.has(id))
        errors.push(
          `unregistered ${entry.kind} owner required at consumer ${path}: ${entry.signature}`,
        );
    }
  for (const [id, entry] of legacy) {
    if ((!requireAllLegacy && !files.has(entry.path)) || !requireAllLegacy)
      continue;
    if ((counts.get(id) ?? 0) !== entry.count)
      errors.push(
        `legacy owner ${entry.key} count differs at consumer ${entry.path}: ${entry.signature}`,
      );
  }
  return errors;
}
export function checkLayoutContract(root = webRoot) {
  generateLayoutContract(root, true);
  const errors = checkConsumers(readLayoutContract(root), sourceFiles(root));
  if (errors.length) throw new Error(errors.join("\n"));
}
if (
  process.argv[1] &&
  resolve(process.argv[1]) === fileURLToPath(import.meta.url)
)
  checkLayoutContract();

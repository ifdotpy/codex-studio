import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

export const RATCHET_BASELINE = fileURLToPath(
  new URL("./ratchet-baseline.json", import.meta.url),
);

export type Violation = { file: string; fingerprint: string; message: string };
export type BaselineEntry = Pick<Violation, "file" | "fingerprint">;
export type Baseline = Record<string, BaselineEntry[]>;

// Call in source order. Occurrence indices distinguish identical matches in a file.
export function fingerprintMatches(
  matches: { file: string; text: string; message: string }[],
): Violation[] {
  const occurrences = new Map<string, number>();
  return matches.map(({ file, text, message }) => {
    const normalized = text.trim().replace(/\s+/g, " ");
    const digest = createHash("sha256").update(normalized).digest("hex");
    const key = `${file}\0${digest}`;
    const index = occurrences.get(key) ?? 0;
    occurrences.set(key, index + 1);
    return { file, fingerprint: `${digest}:${index}`, message };
  });
}

export function checkRatchet(
  ruleId: string,
  violations: Violation[],
  baseline: Baseline = JSON.parse(readFileSync(RATCHET_BASELINE, "utf8")),
): string[] {
  const key = (entry: BaselineEntry) => `${entry.file}\0${entry.fingerprint}`;
  const expected = new Set((baseline[ruleId] ?? []).map(key));
  const actual = new Set(violations.map(key));
  return [
    ...violations
      .filter((entry) => !expected.has(key(entry)))
      .map(
        (entry) =>
          `${ruleId}: new violation in ${entry.file}: ${entry.message} (${entry.fingerprint})`,
      ),
    ...(baseline[ruleId] ?? [])
      .filter((entry) => !actual.has(key(entry)))
      .map(
        (entry) =>
          `${ruleId}: fixed violation in ${entry.file}; shrink the baseline (${entry.fingerprint})`,
      ),
  ];
}

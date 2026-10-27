import { execFileSync } from "node:child_process";
import { readdirSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import {
  checkRatchet,
  fingerprintMatches,
  readBaseline,
  RATCHET_BASELINE,
  type Baseline,
  type Violation,
} from "./ratchet.ts";

const root = fileURLToPath(new URL("../../../..", import.meta.url));
const sg = fileURLToPath(
  new URL("../../../../node_modules/.bin/ast-grep", import.meta.url),
);

export function scan(): Record<string, Violation[]> {
  const paths = execFileSync("git", ["ls-files", "-z"], {
    cwd: root,
    encoding: "utf8",
  })
    .split("\0")
    .filter((file) => /\.(rs|ts|tsx)$/.test(file));
  const output: unknown = JSON.parse(
    execFileSync(sg, ["scan", "--json=compact", ...paths], {
      cwd: root,
      encoding: "utf8",
      maxBuffer: 32 * 1024 * 1024,
    }),
  );
  if (!Array.isArray(output)) throw new Error("Invalid ast-grep output");
  const matches: {
    ruleId: string;
    file: string;
    text: string;
    message: string;
    offset: number;
  }[] = output.map((value: unknown) => {
    if (
      !value ||
      typeof value !== "object" ||
      !("ruleId" in value) ||
      typeof value.ruleId !== "string" ||
      !("file" in value) ||
      typeof value.file !== "string" ||
      !("text" in value) ||
      typeof value.text !== "string" ||
      !("message" in value) ||
      typeof value.message !== "string" ||
      !("range" in value) ||
      !value.range ||
      typeof value.range !== "object" ||
      !("byteOffset" in value.range) ||
      !value.range.byteOffset ||
      typeof value.range.byteOffset !== "object" ||
      !("start" in value.range.byteOffset) ||
      typeof value.range.byteOffset.start !== "number"
    )
      throw new Error("Invalid ast-grep match");
    return {
      ruleId: value.ruleId,
      file: value.file,
      text: value.text,
      message: value.message,
      offset: value.range.byteOffset.start,
    };
  });
  matches.sort((a, b) => a.file.localeCompare(b.file) || a.offset - b.offset);
  const grouped: Record<string, Violation[]> = {};
  for (const rule of new Set(matches.map((match) => match.ruleId)))
    grouped[rule] = fingerprintMatches(
      matches.filter((match) => match.ruleId === rule),
    );
  return grouped;
}

export function run(shrink = false): number {
  const found = scan();
  const baseline = readBaseline();
  const rules = new Set(
    readdirSync(fileURLToPath(new URL("./ast-grep/rules", import.meta.url)))
      .filter((file) => file.endsWith(".yml"))
      .map((file) => file.slice(0, -4)),
  );
  if (shrink) {
    const updated: Baseline = { ...baseline };
    for (const rule of rules) {
      const actual = new Set(
        (found[rule] ?? []).map(
          (entry) => `${entry.file}\0${entry.fingerprint}`,
        ),
      );
      updated[rule] = (baseline[rule] ?? [])
        .filter((entry) => actual.has(`${entry.file}\0${entry.fingerprint}`))
        .sort(
          (a, b) =>
            a.file.localeCompare(b.file) ||
            a.fingerprint.localeCompare(b.fingerprint),
        );
    }
    writeFileSync(RATCHET_BASELINE, `${JSON.stringify(updated, null, 2)}\n`);
  }
  const errors = [...rules].flatMap((rule) =>
    checkRatchet(rule, found[rule] ?? [], shrink ? readBaseline() : baseline),
  );
  for (const error of errors) console.error(error);
  return errors.length ? 1 : 0;
}
if (process.argv[1] === fileURLToPath(import.meta.url))
  process.exitCode = run(process.argv[2] === "--shrink");

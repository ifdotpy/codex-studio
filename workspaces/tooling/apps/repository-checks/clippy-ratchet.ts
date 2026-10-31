import { spawn } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { createInterface } from "node:readline";
import { once } from "node:events";
import { CARGO_BUILD_JOBS } from "./gate-config.ts";
import {
  checkRatchet,
  fingerprintMatches,
  readBaseline,
  RATCHET_BASELINE,
  type Violation,
} from "./ratchet.ts";

const root = fileURLToPath(new URL("../../../..", import.meta.url));
const lints = [
  "struct_excessive_bools",
  "option_option",
  "redundant_clone",
  "needless_pass_by_value",
  "from_value",
  "to_value",
];
const runtimeMethods = [
  ...readFileSync(
    path.join(root, "workspaces/runtime/clippy.toml"),
    "utf8",
  ).matchAll(/path = "([^"]+)"/g),
]
  .map((match) => match[1])
  .filter((value): value is string => value !== undefined);
const scoped = /^workspaces\/runtime\/packages\/runtime-(services|store)\//;

type Match = {
  rule: string;
  file: string;
  text: string;
  message: string;
  start: number;
  end: number;
};
export function collector() {
  const matches = new Map<string, Match>();
  function add(line: string) {
    if (!line) return;
    const item = JSON.parse(line);
    if (item.reason !== "compiler-message") return;
    const diagnostic = item.message;
    const code: unknown = diagnostic.code?.code;
    if (typeof code !== "string" || !code.startsWith("clippy::")) {
      if (diagnostic.level === "warning" || diagnostic.level === "error")
        throw new Error(diagnostic.rendered ?? diagnostic.message);
      return;
    }
    let rule = code.slice(8);
    if (rule === "disallowed_methods") {
      if (diagnostic.message.includes("serde_json::from_value"))
        rule = "from_value";
      else if (diagnostic.message.includes("serde_json::to_value"))
        rule = "to_value";
      else {
        const span = diagnostic.spans.find(
          (value: { is_primary: boolean }) => value.is_primary,
        );
        const file = span?.file_name;
        if (
          typeof file === "string" &&
          (file.startsWith("workspaces/runtime/packages/runtime-http/") ||
            (file.startsWith("workspaces/runtime/") &&
              runtimeMethods.some((method) =>
                diagnostic.message.includes(`\`${method}\``),
              )))
        )
          throw new Error(diagnostic.rendered ?? diagnostic.message);
        return;
      }
    }
    if (!lints.includes(rule)) {
      if (diagnostic.level === "warning" || diagnostic.level === "error")
        throw new Error(diagnostic.rendered ?? diagnostic.message);
      return;
    }
    const span = diagnostic.spans.find(
      (value: { is_primary: boolean }) => value.is_primary,
    );
    if (
      !span ||
      typeof span.file_name !== "string" ||
      !Number.isInteger(span.byte_start) ||
      !Number.isInteger(span.byte_end)
    )
      throw new Error(`Invalid Clippy span for ${rule}`);
    if (
      (rule === "from_value" || rule === "to_value") &&
      !scoped.test(span.file_name)
    )
      return;
    const file = path.normalize(span.file_name);
    if (path.isAbsolute(file) || file.split("/").includes(".."))
      throw new Error(
        `Clippy reported a non-repository span for ${rule}: ${file}`,
      );
    const start: number = span.byte_start;
    const end: number = span.byte_end;
    const text = readFileSync(path.join(root, file))
      .subarray(start, end)
      .toString("utf8");
    const match = {
      rule,
      file,
      text: `${rule} ${text}`,
      message: diagnostic.message,
      start,
      end,
    };
    matches.set(`${rule}\0${file}\0${start}\0${end}`, match);
  }
  function finish(): Record<string, Violation[]> {
    const sorted = [...matches.values()].sort(
      (a, b) =>
        a.file.localeCompare(b.file) || a.start - b.start || a.end - b.end,
    );
    return Object.fromEntries(
      lints.map((rule) => [
        rule,
        fingerprintMatches(sorted.filter((match) => match.rule === rule)),
      ]),
    );
  }
  return { add, finish };
}

export function collect(output: string): Record<string, Violation[]> {
  const result = collector();
  for (const line of output.split("\n")) result.add(line);
  return result.finish();
}

export async function run(shrink = false): Promise<number> {
  const args = [
    "clippy",
    "--workspace",
    "--all-targets",
    "--locked",
    "-j",
    String(CARGO_BUILD_JOBS),
    "--message-format=json",
    "--",
    "-D",
    "warnings",
    "--cap-lints",
    "warn",
    ...[
      "struct_excessive_bools",
      "option_option",
      "redundant_clone",
      "needless_pass_by_value",
      "disallowed_methods",
    ].flatMap((lint) => ["-W", `clippy::${lint}`]),
  ];
  const child = spawn("cargo", args, {
    cwd: root,
    stdio: ["ignore", "pipe", "inherit"],
    env: {
      ...process.env,
      CLIPPY_CONF_DIR: fileURLToPath(
        new URL("./clippy-ratchet", import.meta.url),
      ),
    },
  });
  const exit = once(child, "close");
  const scan = collector();
  const failures: unknown[] = [];
  for await (const line of createInterface({ input: child.stdout })) {
    try {
      scan.add(line);
    } catch (error) {
      failures.push(error);
    }
  }
  const [status] = await exit;
  if (status !== 0) throw new Error(`cargo clippy exited ${status}`);
  if (failures.length)
    throw new Error(
      failures
        .map((error) =>
          error instanceof Error ? error.message : String(error),
        )
        .join("\n\n"),
    );
  const found = scan.finish();
  const baseline = readBaseline();
  if (shrink) {
    for (const rule of lints) {
      const actual = new Set(
        (found[rule] ?? []).map(
          (entry) => `${entry.file}\0${entry.fingerprint}`,
        ),
      );
      baseline[rule] = (baseline[rule] ?? []).filter((entry) =>
        actual.has(`${entry.file}\0${entry.fingerprint}`),
      );
    }
    writeFileSync(RATCHET_BASELINE, `${JSON.stringify(baseline, null, 2)}\n`);
  }
  const errors = lints.flatMap((rule) =>
    checkRatchet(rule, found[rule] ?? [], baseline),
  );
  errors.forEach((error) => console.error(error));
  return errors.length ? 1 : 0;
}
if (process.argv[1] === fileURLToPath(import.meta.url))
  process.exitCode = await run(process.argv[2] === "--shrink");

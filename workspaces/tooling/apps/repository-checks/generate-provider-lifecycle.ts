// Selected vendor leaves only. Run from the repository root with Node.
import { execFileSync } from "node:child_process";
import {
  mkdtempSync,
  mkdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import os from "node:os";
import path from "node:path";

const version = "codex-cli 0.164.0";
if (
  execFileSync("codex", ["--version"], { encoding: "utf8" }).trim() !== version
)
  throw new Error(`Lifecycle leaves require ${version}`);
const directory = mkdtempSync(path.join(os.tmpdir(), "studio-lifecycle-"));
const bridge = "workspaces/providers/apps/claude-bridge/codex-generated";
const schema =
  "workspaces/runtime/packages/runtime-schema/src/schemas/codex/lifecycle.schema.json";
try {
  for (const format of ["ts", "json-schema"]) {
    execFileSync("codex", [
      "app-server",
      `generate-${format}`,
      "--experimental",
      "--out",
      path.join(directory, format),
    ]);
  }
  const turn: { definitions: Record<string, unknown> } = JSON.parse(
    readFileSync(
      path.join(directory, "json-schema/v2/TurnCompletedNotification.json"),
      "utf8",
    ),
  );
  const thread: { definitions: Record<string, unknown> } = JSON.parse(
    readFileSync(
      path.join(
        directory,
        "json-schema/v2/ThreadStatusChangedNotification.json",
      ),
      "utf8",
    ),
  );
  const names = [
    "TurnStatus",
    "ThreadStatus",
    "ThreadActiveFlag",
    "TurnItemsView",
  ];
  const definitions = Object.fromEntries(
    names.map((name) => {
      const leaf = turn.definitions[name] ?? thread.definitions[name];
      if (leaf === undefined) throw new Error(`Missing vendor leaf ${name}`);
      return [name, leaf];
    }),
  );
  mkdirSync(bridge, { recursive: true });
  for (const name of names) {
    const source = readFileSync(
      path.join(directory, `ts/v2/${name}.ts`),
      "utf8",
    ).replaceAll('from "./ThreadActiveFlag"', 'from "./ThreadActiveFlag.ts"');
    writeFileSync(
      `${bridge}/${name}.ts`,
      `// Owner: ${version}, app-server generate-ts --experimental.\n${source}`,
    );
  }
  writeFileSync(
    schema,
    JSON.stringify(
      {
        $comment: `Owner: ${version}; app-server generate-json-schema --experimental. Selected definitions, unchanged.`,
        definitions,
      },
      null,
      2,
    ) + "\n",
  );
  execFileSync("pnpm", ["exec", "oxfmt", "--write", bridge, schema], {
    stdio: "inherit",
  });
} finally {
  rmSync(directory, { recursive: true, force: true });
}

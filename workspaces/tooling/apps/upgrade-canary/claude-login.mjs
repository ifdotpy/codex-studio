#!/usr/bin/env node
// Metadata-only admission. Studio's existing Add Claude button owns authentication.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { parseArgs } from "node:util";
import { fileURLToPath } from "node:url";
import {
  OWNERSHIP_DIRECTORY,
  reserveStudioProfile,
  bindStudioProfile,
} from "./studio-claude-profile.mjs";

export function cli(argv) {
  const { values } = parseArgs({
    args: argv,
    options: {
      reserve: { type: "boolean", default: false },
      bind: { type: "boolean", default: false },
      record: {
        type: "string",
        default: path.join(OWNERSHIP_DIRECTORY, "studio-add.json"),
      },
      "login-id": { type: "string" },
      "live-state": {
        type: "string",
        default: path.join(os.homedir(), ".local/state/codex-agents"),
      },
    },
  });
  if (values.reserve === values.bind)
    throw new Error(
      "Select --reserve before Add Claude, or --bind after the fresh account is excluded.",
    );
  const record = path.resolve(values.record),
    live = fs.realpathSync(path.resolve(values["live-state"]));
  const result = values.reserve
    ? reserveStudioProfile(record, live)
    : bindStudioProfile(record, values["login-id"], { liveState: live });
  if (result.liveState !== live)
    throw new Error("The reservation names another state directory.");
  process.stdout.write(
    `Canary profile ${result.phase}: ${record}\nNo authentication or credential access was performed.\n`,
  );
  return 0;
}
if (
  process.argv[1] &&
  fileURLToPath(import.meta.url) === fs.realpathSync(process.argv[1])
) {
  try {
    process.exitCode = cli(process.argv.slice(2));
  } catch {
    process.stderr.write(
      "Canary profile admission refused. Use a pre-Add reservation and one fresh, excluded Studio account.\n",
    );
    process.exitCode = 1;
  }
}

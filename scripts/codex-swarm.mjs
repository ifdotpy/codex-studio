#!/usr/bin/env node
import { realpathSync } from "node:fs";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

const shim = realpathSync(process.argv[1]);
const source = resolve(
  shim,
  "..",
  "..",
  "workspaces/runtime/apps/server/src/codex-swarm.mjs",
);
process.argv[1] = source;
await import(pathToFileURL(source).href);

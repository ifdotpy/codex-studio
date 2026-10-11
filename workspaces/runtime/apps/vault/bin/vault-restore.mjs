#!/usr/bin/env node
// Rebuild one state of a vault into a folder (the latest by default).
//
//   node vault-restore.mjs --project <prefix> --out <folder> [--seq <n>]
import {
  mkdirSync,
  symlinkSync,
  writeFileSync,
  chmodSync,
  existsSync,
  readdirSync,
} from "node:fs";
import { dirname, join, resolve } from "node:path";
import { parseArgs } from "node:util";
import { reader } from "../src/local.mjs";

const { values: args } = parseArgs({
  options: {
    project: { type: "string" },
    out: { type: "string" },
    seq: { type: "string" },
  },
});
if (!args.project || !args.out) {
  console.error(
    "usage: vault-restore --project <prefix> --out <folder> [--seq <n>]",
  );
  process.exit(2);
}
const out = resolve(args.out);
if (existsSync(out) && readdirSync(out).length) {
  console.error(`${out} is not empty`);
  process.exit(1);
}
const vault = await reader(args.project);
const files = await vault.files(args.seq ? Number(args.seq) : undefined);
let count = 0;
for (const [path, entry] of Object.entries(files)) {
  const target = join(out, path);
  if (!target.startsWith(out + "/")) throw new Error(`unsafe path ${path}`);
  mkdirSync(dirname(target), { recursive: true });
  const data = await vault.read(entry);
  if (entry[0] === "120000")
    symlinkSync(new TextDecoder().decode(data), target);
  else {
    writeFileSync(target, data);
    chmodSync(target, entry[0] === "100755" ? 0o755 : 0o644);
  }
  count += 1;
}
console.error(`${count} files of state ${args.seq ?? "latest"} in ${out}`);

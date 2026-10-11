#!/usr/bin/env node
// Serve the latest state of a vault read-only over WebDAV on localhost, for Finder
// (Go > Connect to Server, http://localhost:<port>/). The first start asks for the paper key and
// keeps the vault identity in the macOS Keychain.
//
//   node vault-view.mjs --project <prefix> [--port 8095] [--mount]
import { execFileSync } from "node:child_process";
import { homedir } from "node:os";
import { join } from "node:path";
import { parseArgs } from "node:util";
import { reader } from "../src/local.mjs";
import { view } from "../src/webdav.mjs";

const { values: args } = parseArgs({
  options: {
    project: { type: "string" },
    port: { type: "string", default: "8095" },
    mount: { type: "boolean", default: false },
  },
});
if (!args.project) {
  console.error("usage: vault-view --project <prefix> [--port 8095] [--mount]");
  process.exit(2);
}
let vault = await reader(args.project, { keep: true });
const server = view({
  async load() {
    // A new reader lists the states again, so the view moves to the newest one.
    vault = await reader(args.project, { keep: true });
    const latest = vault.latest();
    const { header } = await vault.tree(latest.seq, latest.attempt);
    return { files: await vault.files(), time: header.time };
  },
  read: (entry) => vault.read(entry),
  cache: join(homedir(), "Library/Caches/studio-vault", args.project),
});
server.listen(Number(args.port), "127.0.0.1", () => {
  const url = `http://localhost:${args.port}/`;
  console.error(`vault ${args.project}: ${url} (read only)`);
  if (args.mount) execFileSync("osascript", ["-e", `mount volume "${url}"`]);
});

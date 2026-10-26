import assert from "node:assert/strict";
import * as fs from "node:fs";
const value: unknown = JSON.parse(fs.readFileSync(0, "utf8"));
assert(value && typeof value === "object" && "servers" in value);
const servers = value.servers;
assert(servers && typeof servers === "object" && "fixture" in servers);
const fixture = servers.fixture;
if (
  process.argv[2] !== "--import-server-credentials" ||
  !fixture ||
  typeof fixture !== "object" ||
  !("privateKey" in fixture) ||
  fixture.privateKey !== "private-fixture"
)
  process.exit(2);
fs.writeFileSync(process.argv[3] + "/proof", "received through stdin");

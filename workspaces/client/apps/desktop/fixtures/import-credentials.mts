import * as fs from "node:fs";
// This fixture reads the document written by exportServerCredentials.
const value: Record<
  "servers",
  Record<string, Record<"privateKey", string>>
> = JSON.parse(fs.readFileSync(0, "utf8"));
if (
  process.argv[2] !== "--import-server-credentials" ||
  value.servers.fixture.privateKey !== "private-fixture"
)
  process.exit(2);
fs.writeFileSync(process.argv[3] + "/proof", "received through stdin");

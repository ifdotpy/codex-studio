// The vault gateway process (docs/vault.md, "Gateway"). sync.py writes the configuration and runs
// it under systemd: TLS files come as credentials, and it listens on the tailnet address only.
import { S3Client } from "@aws-sdk/client-s3";
import { fromTemporaryCredentials } from "@aws-sdk/credential-providers";
import { randomBytes } from "node:crypto";
import { readFileSync, statSync } from "node:fs";
import https from "node:https";
import { join } from "node:path";
import { makeVault } from "../src/service.mjs";
import { makeGateway } from "./server.mjs";

const CONFIG =
  process.env.VAULT_GATEWAY_CONFIG ?? "/etc/vault-gateway/config.json";
// Other values only for a local check of the bundle.
const PORT = Number(process.env.VAULT_GATEWAY_PORT ?? 443);
const site = new URL("./site", import.meta.url).pathname;
const credentials = process.env.CREDENTIALS_DIRECTORY;

let loaded = { mtime: -1, value: {} };
function config() {
  const mtime = statSync(CONFIG).mtimeMs;
  if (mtime !== loaded.mtime)
    loaded = { mtime, value: JSON.parse(readFileSync(CONFIG, "utf8")) };
  return loaded.value;
}

// Tokens live 10 minutes; a restart only asks the browsers for a new challenge.
const secret = randomBytes(32);
const vaults = new Map();
function vaultFor(name) {
  const project = config().projects?.[name];
  if (!project) return null;
  const key = JSON.stringify([name, project]);
  if (!vaults.has(key)) {
    const aws = { region: project.region, useDualstackEndpoint: true };
    const s3 = new S3Client({
      ...aws,
      credentials: fromTemporaryCredentials({
        params: { RoleArn: project.role, RoleSessionName: "vault-gateway" },
        clientConfig: aws,
      }),
    });
    vaults.set(
      key,
      makeVault({
        s3,
        bucket: project.bucket,
        prefix: name,
        recipient: project.recipient,
        secret,
      }),
    );
  }
  return vaults.get(key);
}

const { listen } = config();
if (!listen) throw new Error(`${CONFIG} has no listen address yet`);
const server = https.createServer(
  {
    cert: readFileSync(join(credentials, "tls.crt")),
    key: readFileSync(join(credentials, "tls.key")),
  },
  makeGateway({ config, vaultFor, site }),
);
server.listen(PORT, listen, () =>
  console.log(`vault gateway on https://${config().host} (${listen})`),
);

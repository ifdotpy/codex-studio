// The Mac side of a vault: S3 through the AWS profile of cloud.json, and the vault identity from
// the macOS Keychain or from the paper key (docs/vault.md, "Mac view").
import {
  GetObjectCommand,
  ListObjectsV2Command,
  S3Client,
} from "@aws-sdk/client-s3";
import { fromIni } from "@aws-sdk/credential-providers";
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { createInterface } from "node:readline";
import { openSlot } from "./keys.mjs";
import { VaultReader } from "./reader.mjs";

const SERVICE = "studio-vault";

export function cloud(prefix) {
  return JSON.parse(
    readFileSync(
      join(homedir(), ".config/studio-vault", prefix, "cloud.json"),
      "utf8",
    ),
  );
}

export function store(prefix) {
  const { profile, region, bucket } = cloud(prefix);
  const s3 = new S3Client({ region, credentials: fromIni({ profile }) });
  return {
    async list(sub) {
      const keys = [];
      let token;
      do {
        const page = await s3.send(
          new ListObjectsV2Command({
            Bucket: bucket,
            Prefix: `${prefix}/${sub}`,
            ContinuationToken: token,
          }),
        );
        keys.push(...(page.Contents ?? []).map((o) => o.Key));
        token = page.NextContinuationToken;
      } while (token);
      return keys;
    },
    async get(key) {
      const object = await s3.send(
        new GetObjectCommand({ Bucket: bucket, Key: key }),
      );
      return object.Body.transformToByteArray();
    },
  };
}

function ask(question) {
  const terminal = createInterface({
    input: process.stdin,
    output: process.stderr,
    terminal: true,
  });
  return new Promise((resolve) =>
    terminal.question(
      question,
      (answer) => (terminal.close(), resolve(answer)),
    ),
  );
}

/** The vault identity: from the Keychain, else from the paper key (then kept if `keep`). */
export async function identity(prefix, data, { keep = false } = {}) {
  try {
    return execFileSync(
      "security",
      ["find-generic-password", "-s", SERVICE, "-a", prefix, "-w"],
      { stdio: ["ignore", "pipe", "ignore"] },
    )
      .toString()
      .trim();
  } catch {
    // Not kept yet.
  }
  const paper = (await ask(`Paper key of the vault "${prefix}": `))
    .replace(/\s+/g, "")
    .toUpperCase();
  const vault = await openSlot(
    await data.get(`${prefix}/slots/paper.age`),
    paper,
  );
  if (keep)
    execFileSync("security", [
      "add-generic-password",
      "-U",
      "-s",
      SERVICE,
      "-a",
      prefix,
      "-w",
      vault,
    ]);
  return vault;
}

export async function reader(prefix, options) {
  const data = store(prefix);
  const vault = await identity(prefix, data, options);
  return new VaultReader({
    prefix,
    keys: await data.list("states/"),
    get: (k) => data.get(k),
    identity: vault,
  });
}

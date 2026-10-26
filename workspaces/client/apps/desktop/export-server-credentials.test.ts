import { nodeFixture } from "./test-support.ts";
import { test } from "vitest";
import assert from "node:assert/strict";
import { mkdtemp, writeFile, readFile, rm } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { exportServerCredentials } from "./export-server-credentials.ts";

test("credential migration uses stdin and leaves the encrypted source intact", async () => {
  const profile = await mkdtemp(path.join(os.tmpdir(), "studio-keys-"));
  try {
    const source = path.join(profile, "paired-servers.json");
    const encrypted = Buffer.from("encrypted fixture").toString("base64");
    await writeFile(source, encrypted);
    const safeStorage = {
      isEncryptionAvailable: () => true,
      decryptString: (bytes: Buffer) => {
        assert.equal(bytes.toString(), "encrypted fixture");
        return JSON.stringify({
          drafts: {},
          servers: { fixture: { privateKey: "private-fixture" } },
        });
      },
    };
    await assert.rejects(
      exportServerCredentials({ profile, destination: undefined, safeStorage }),
      /destination/,
    );
    const executable = path.join(profile, "importer");
    await writeFile(executable, nodeFixture("import-credentials"), {
      mode: 0o700,
    });
    const destination = { executable, profile };
    await exportServerCredentials({ profile, destination, safeStorage });
    assert.equal(
      await readFile(path.join(profile, "proof"), "utf8"),
      "received through stdin",
    );
    assert.equal(await readFile(source, "utf8"), encrypted);
    await writeFile(executable, nodeFixture("import-credentials-failure"), {
      mode: 0o700,
    });
    await assert.rejects(
      exportServerCredentials({ profile, destination, safeStorage }),
      (error: unknown) => {
        assert(error instanceof Error);
        assert.match(error.message, /Native key import failed/);
        assert.ok(!error.message.includes("private-fixture"));
        return true;
      },
    );
  } finally {
    await rm(profile, { recursive: true, force: true });
  }
});

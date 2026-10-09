import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { playwrightScratchRoot, spawnFixture, test } from "./playwright.mjs";

const repositoryRoot = dirname(
  dirname(dirname(fileURLToPath(import.meta.url))),
);

async function fixtureAccountStatus(env = {}) {
  const state = await mkdtemp(join(playwrightScratchRoot, "fixture-account-"));
  const fixture = spawnFixture(
    "python3",
    ["-B", join(repositoryRoot, "tests/simple-ui-fixture.py"), state],
    { stdio: ["pipe", "pipe", "pipe"], env },
  );
  let log = "";
  fixture.stderr.on("data", (chunk) => (log += String(chunk)));
  try {
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", () => reject(new Error(log)));
    });
    const response = await fetch(`http://127.0.0.1:${port}/api/accounts`);
    assert.equal(response.status, 200);
    const body = await response.json();
    return body.accounts.find((account) => account.id === "default")?.status;
  } finally {
    fixture.kill();
  }
}

test("spawnFixture defaults to a synthetic account and supports signed-out opt-out", async () => {
  assert.equal(await fixtureAccountStatus(), "ready");
  assert.equal(
    await fixtureAccountStatus({ CODEX_PLAYWRIGHT_SIGNED_OUT: "1" }),
    "signedOut",
  );
});

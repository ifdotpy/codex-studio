import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import {
  getFixturePythonCommand,
  playwrightScratchRoot,
  spawnFixture,
  test,
} from "./playwright.mjs";

const repositoryRoot = resolve(
  dirname(fileURLToPath(import.meta.url)),
  "../../../../..",
);

async function fixtureAccountStatus(env = {}) {
  const state = await mkdtemp(join(playwrightScratchRoot, "fixture-account-"));
  const fixture = spawnFixture(
    "python3",
    [
      "-B",
      join(
        repositoryRoot,
        "workspaces/runtime/apps/server/tests/simple-ui-fixture.py",
      ),
      state,
    ],
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

test("global setup resolves an equipped fixture Python without caller variables", () => {
  const python = getFixturePythonCommand();
  assert.equal(python.startsWith("/"), true);
  const result = spawnSync(
    python,
    ["-c", "import fastapi, httpx, pydantic, uvicorn, watchdog"],
    { encoding: "utf8" },
  );
  assert.equal(result.status, 0, result.stderr);
});

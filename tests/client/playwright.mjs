import { spawn } from "node:child_process";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

if (!process.env.CODEX_STUDIO_TEST_CACHE_ROOT) {
  const cacheRoot = mkdtempSync(
    join(tmpdir(), "codex-studio-playwright-cache-"),
  );
  process.env.CODEX_STUDIO_TEST_CACHE_ROOT = cacheRoot;
  process.env.XDG_CACHE_HOME = cacheRoot;
  process.once("exit", () =>
    rmSync(cacheRoot, { recursive: true, force: true }),
  );
}

const require = createRequire(
  fileURLToPath(new URL("../../web/package.json", import.meta.url)),
);
const { test: baseTest, expect } = require("@playwright/test");
const { chromium } = require("playwright");

const generatedApiSchema = readFileSync(
  new URL("../../web/src/generated/apiSchema.ts", import.meta.url),
  "utf8",
);

export const API_SCHEMA_HASH_HEADER = readGeneratedString(
  "API_SCHEMA_HASH_HEADER",
);
export const API_SCHEMA_HASH_PARAM = readGeneratedString(
  "API_SCHEMA_HASH_PARAM",
);

function readGeneratedString(name) {
  const match = generatedApiSchema.match(
    new RegExp(`${name}\\s*=\\s*"([^"]+)"`),
  );
  if (!match) throw new Error(`Generated ${name} is missing.`);
  return match[1];
}

export function readApiSchemaHash() {
  const match = generatedApiSchema.match(
    /API_SCHEMA_HASH\s*=\s*"([0-9a-f]{64})"/,
  );
  if (!match) throw new Error("Generated API schema hash is missing.");
  return match[1];
}

export function apiSchemaHandshakeSse(body = "", overrides = {}) {
  return `event: api-schema\ndata: ${JSON.stringify(apiSchemaHandshakeEvent(overrides))}\n\n${body}`;
}

export function apiSchemaHandshakeEvent(overrides = {}) {
  return { hash: readApiSchemaHash(), ...overrides };
}

export function protocol3SseEvent(name, value) {
  return `event: ${name}\ndata: ${JSON.stringify(value)}\n\n`;
}

const fixtureScopes = new WeakMap();
const childTerminationWaitMs = 1_000;
const fixtureCleanupTimeoutMs = 5_000;

async function waitForChildExit(child) {
  if (child.exitCode !== null || child.signalCode !== null) return;
  await new Promise((resolve) => {
    const timer = setTimeout(() => {
      child.off("close", onClose);
      resolve();
    }, childTerminationWaitMs);
    timer.unref?.();
    const onClose = () => {
      clearTimeout(timer);
      resolve();
    };
    child.once("close", onClose);
  });
}

async function waitForChildStart(child) {
  if (
    child.pid !== undefined ||
    child.exitCode !== null ||
    child.signalCode !== null
  )
    return;
  await new Promise((resolve) => {
    const finish = () => {
      clearTimeout(timer);
      child.off("spawn", finish);
      child.off("error", finish);
      child.off("close", finish);
      resolve();
    };
    const timer = setTimeout(finish, childTerminationWaitMs);
    timer.unref?.();
    child.once("spawn", finish);
    child.once("error", finish);
    child.once("close", finish);
  });
}

async function stopFixtureChild(child) {
  await waitForChildStart(child);
  if (child.pid === undefined) return;
  if (child.exitCode !== null || child.signalCode !== null) return;
  child.kill("SIGTERM");
  await waitForChildExit(child);
  if (child.exitCode === null && child.signalCode === null) {
    child.kill("SIGKILL");
    await waitForChildExit(child);
  }
  if (child.exitCode === null && child.signalCode === null)
    throw new Error(`Fixture process ${child.pid} did not exit after SIGKILL`);
}

const test = baseTest.extend({
  fixtureChildCleanup: [
    // oxlint-disable-next-line no-empty-pattern
    async ({}, use, testInfo) => {
      const scope = { children: new Set(), errors: [], closing: false };
      fixtureScopes.set(testInfo, scope);
      let testFailed = false;
      let testError;
      try {
        await use();
      } catch (error) {
        testFailed = true;
        testError = error;
      }
      scope.closing = true;
      const results = await Promise.allSettled(
        [...scope.children].map(stopFixtureChild),
      );
      fixtureScopes.delete(testInfo);
      const cleanupFailures = [
        ...scope.errors,
        ...results
          .filter((result) => result.status === "rejected")
          .map((result) => result.reason),
      ];
      if (cleanupFailures.length)
        throw new AggregateError(
          testFailed ? [testError, ...cleanupFailures] : cleanupFailures,
          "Fixture child process failed or cleanup did not complete",
        );
      if (testFailed) throw testError;
    },
    { auto: true, timeout: fixtureCleanupTimeoutMs },
  ],
});

export { test, expect };

export function spawnFixture(command, args, options) {
  let testInfo;
  try {
    testInfo = baseTest.info();
  } catch {
    throw new Error(
      "spawnFixture must be called during a shared Playwright test",
    );
  }
  const scope = fixtureScopes.get(testInfo);
  if (!scope || scope.closing)
    throw new Error(
      "spawnFixture is only available before test cleanup begins",
    );

  const child = spawn(command, args, options);
  scope.children.add(child);
  child.on("error", (error) => {
    if (error.code !== "ESRCH") scope.errors.push(error);
  });
  child.once("close", () => scope.children.delete(child));
  return child;
}

export const browserExecutablePath =
  process.env.CHROME_BIN?.trim() || chromium.executablePath();

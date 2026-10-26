import type { ServerResponse } from "node:http";
import type { Route } from "@playwright/test";
import type { JsonInput } from "../../src/types.ts";
import { required } from "../teams/helpers.ts";
import { createServer } from "vite";
import type {} from "./structured-operational-errors-ui.fixture.tsx";
import {
  apiSchemaHandshakeSse,
  apiSchemaHeaders,
  syncIdentityFixture,
  syncProtocolFixture,
  test,
} from "../playwright.ts";
// Structured diagnostic fixtures through real React components. No live API writes.
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
test("Structured operational errors ui", async ({
  browser: testBrowser,
  page: runnerPage,
  context: runnerContext,
}) => {
  test.setTimeout(180_000);
  const root = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const temporary = await mkdtemp(join(tmpdir(), "studio-operational-errors-"));
  const syncIdentity = syncIdentityFixture();
  const syncStreams = new Set<ServerResponse>();

  const server = await createServer({
    configFile: false,
    root: join(root, "workspaces/client/apps/web"),
    cacheDir: join(temporary, "vite"),
    plugins: [
      {
        name: "operational-errors-fixture",
        configureServer(server) {
          server.middlewares.use("/api/sync/stream", (request, response) => {
            const resources: import("../playwright.ts").ResourceRef[] =
              JSON.parse(
                new URL(
                  request.url || "/",
                  "http://127.0.0.1",
                ).searchParams.get("resources") || "[]",
              );
            const event = {
              protocol: 4,
              workspaceId: syncIdentity.workspaceId,
              epoch: "fixture",
              revision: 1,
              reason: "initial",
              resources,
              resourceVersions: resources.map((resource) => ({
                resource,
                revision: 1,
              })),
            };
            response.writeHead(200, {
              ...apiSchemaHeaders({ "Content-Type": "text/event-stream" }),
              "Cache-Control": "no-cache",
            });
            response.write(
              apiSchemaHandshakeSse(
                `event: resources\ndata: ${JSON.stringify(event)}\n\n`,
              ),
            );
            response.flushHeaders();
            syncStreams.add(response);
            response.on("close", () => syncStreams.delete(response));
          });
        },
      },
    ],
    server: { host: "127.0.0.1", port: 0, hmr: false },
  });
  await server.listen();
  try {
    const page = runnerPage;
    await page.setViewportSize({ width: 390, height: 844 });
    const writes: unknown[] = [],
      errors: string[] = [],
      consoleErrors: string[] = [];
    page.on("console", (m) => {
      if (m.type() === "error") consoleErrors.push(m.text());
    });
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: '<!doctype html><div id="root"></div><script type="module">import "/tests/errors/structured-operational-errors-ui.fixture.tsx";</script>',
      }),
    );
    let messageFailure: JsonInput;
    await page.route("**/api/**", async (route) => {
      const fulfill = (options: NonNullable<Parameters<Route["fulfill"]>[0]>) =>
        route.fulfill({
          ...options,
          headers: { ...apiSchemaHeaders(), ...options.headers },
        });
      if (route.request().method() === "POST") {
        writes.push(route.request().postDataJSON());
        return fulfill({
          status: 409,
          json: { error: "Unexpected action" },
        });
      }
      const url = new URL(route.request().url());
      if (url.pathname === "/api/sync/stream") return route.continue();
      if (url.pathname === "/api/sync/identity")
        return fulfill({ json: syncIdentity });
      if (url.pathname === "/api/sync/protocol")
        return fulfill({ json: syncProtocolFixture() });
      if (url.pathname === "/api/session")
        return fulfill({ json: { token: "fixture" } });
      if (url.pathname === "/api/sync/pull")
        return fulfill({
          json: {
            workspaceId: syncIdentity.workspaceId,
            checkpoint: { seq: 0 },
            maxSeq: 0,
            documents: [],
            reset: false,
          },
        });
      if (url.pathname === "/api/complaint")
        return fulfill({ status: 503, json: messageFailure });
      if (url.pathname === "/api/workspace/tasks")
        return fulfill({
          json: {
            tasks: [
              {
                id: "tool",
                agent: "lead",
                kind: "command",
                name: "commandExecution",
                command: "fixture command",
                status: "running",
                created: 1,
              },
            ],
            cursor: { updated: 1, id: "tool" },
            hasMore: false,
            nextBefore: null,
            reset: false,
          },
        });
      if (url.pathname === "/api/task")
        return fulfill({
          json: {
            id: "tool",
            agent: "lead",
            kind: "command",
            name: "commandExecution",
            command: "fixture command",
            status: "running",
            created: 1,
            error: value,
            stdinError: value,
          },
        });
      return fulfill({
        json: { tasks: [], monitors: [], requests: [], id: "tool" },
      });
    });
    await page.goto(required(server.resolvedUrls?.local[0]) + "check");
    await page.waitForFunction(
      () => !!window.structuredOperationalErrorsUiRenderCase,
    );
    const value = {
      error: "Structured operation failed",
      code: "access_failed",
      metadata: { retryable: false, attempt: 7 },
    };
    messageFailure = value;
    for (const which of [
      "request",
      "message",
      "background",
      "capacity",
      "safety",
      "dictation",
    ]) {
      await page.evaluate(
        ({ which, value }) =>
          window.structuredOperationalErrorsUiRenderCase(which, value),
        {
          which,
          value,
        },
      );
      if (which === "dictation")
        await page
          .getByRole("button", { name: "Dictation", exact: true })
          .click();
      if (which === "background")
        await page.locator('[data-task="tool"]').click();
      try {
        const diagnostic =
          which === "dictation"
            ? page.locator(".dictation-item .dictation-error")
            : which === "request"
              ? page.getByText("Structured operation failed", { exact: true })
              : page.getByText("Structured operation failed", { exact: true });
        await diagnostic.first().waitFor({ timeout: 5000 });
      } catch (error) {
        console.error(
          which,
          await page.locator("body").innerText(),
          await page.evaluate(
            () => window.structuredOperationalErrorsUiCrashes,
          ),
        );
        throw error;
      }
      if (which !== "request") {
        await page
          .getByRole("button", { name: "Error details", exact: true })
          .first()
          .click();
        assert.match(await page.locator("body").innerText(), /access_failed/);
        assert.match(await page.locator("body").innerText(), /"attempt": 7/);
      }
      assert.deepEqual(
        await page.evaluate(() => window.structuredOperationalErrorsUiCrashes),
        [],
        which + " retains the component",
      );
      assert.equal(
        writes.length,
        0,
        which + " does not trigger an action to recover",
      );
      assert.equal(
        await page
          .locator("body")
          .innerText()
          .then((text) => text.includes("[object Object]")),
        false,
      );
    }
    await page.evaluate((value) => {
      window.structuredOperationalErrorsUiTranscriptionCalls = 0;
      if (!window.codexDesktop)
        throw new Error("Missing fixture desktop bridge");
      window.codexDesktop.prepareTranscription = async () => {
        throw value;
      };
      window.codexDesktop.transcribeAudio = async () => {
        window.structuredOperationalErrorsUiTranscriptionCalls++;
        return { text: "Must not run", provider: "fixture", onDevice: false };
      };
    }, value);
    await page
      .getByRole("button", { name: "Retry transcription", exact: true })
      .click();
    const deadline = Date.now() + 5000;
    let persisted: Awaited<
      ReturnType<typeof import("../../src/dictation/storage.ts").listRecordings>
    > = [];
    while (Date.now() < deadline) {
      persisted = await page.evaluate(() =>
        window.structuredOperationalErrorsUiReadRecordings(),
      );
      if (
        typeof persisted[0]?.error === "string" &&
        persisted[0]?.error?.includes("E_FIXTURE")
      )
        break;
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
    assert.deepEqual(JSON.parse(required(required(persisted[0]).error)), value);
    assert.equal(required(persisted[0]).id, "recording");
    assert.equal(required(persisted[0]).samples, 4);
    assert.equal(
      await page.evaluate(
        () => window.structuredOperationalErrorsUiTranscriptionCalls,
      ),
      0,
    );
    await page
      .getByText("Structured operation failed", { exact: true })
      .first()
      .waitFor();
    for (const legacy of [
      "Legacy dictation failure",
      "{unparsed diagnostic",
      '"quoted diagnostic"',
    ]) {
      await page.evaluate(
        (value) =>
          window.structuredOperationalErrorsUiRenderCase("dictation", value),
        legacy,
      );
      await page
        .getByRole("button", { name: "Dictation", exact: true })
        .click();
      await page.getByText(legacy, { exact: true }).first().waitFor();
    }
    assert.deepEqual(
      await page.evaluate(() => window.structuredOperationalErrorsUiCrashes),
      [],
    );
    assert.deepEqual(errors, []);
    console.log(
      "PASS: structured request/message/retry/safety/dictation diagnostics preserve render and manual actions",
    );
  } finally {
    for (const stream of syncStreams) stream.end();
    await Promise.all(
      testBrowser
        .contexts()
        .filter((ownedContext) => ownedContext !== runnerContext)
        .map((ownedContext) => ownedContext.close()),
    );
    await server.close();
    await rm(temporary, { recursive: true, force: true });
  }
});

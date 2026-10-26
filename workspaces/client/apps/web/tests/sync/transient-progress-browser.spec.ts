import { fixtureOrigin } from "./helpers.ts";
import type {} from "./transient-progress-browser.fixture.tsx";
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve, join } from "node:path";
import { test } from "../playwright.ts";

for (const flow of ["vault", "auth", "vault-job"]) {
  test(`transient ${flow === "vault-job" ? "vault" : flow} progress has zero ${flow === "vault-job" ? "job receipt" : "idle HTTP"} reads`, async ({
    browser,
  }) => {
    const root = resolve(import.meta.dirname, "../..");
    const { createFixtureViteServer: createServer } =
      await import("../playwright.ts");
    const cacheDir = await mkdtemp(join(tmpdir(), "transient-progress-"));
    const server = await createServer({
      configFile: false,
      root,
      cacheDir,
      server: { host: "127.0.0.1", port: 0 },
      plugins: [
        {
          name: "transient-progress-fixture",
          configureServer(server) {
            server.middlewares.use("/check", (_req, res) => {
              res.setHeader("Content-Type", "text/html");
              res.end(
                '<div id="root"></div><script type="module" src="/tests/sync/transient-progress-browser.fixture.tsx"></script>',
              );
            });
          },
        },
      ],
    });
    const page = await browser.newPage();
    try {
      await server.listen();
      const requests: string[] = [];
      page.on("request", (request) => {
        if (
          /\/api\/vault\/(setup|aws-login|pull)$/.test(
            new URL(request.url()).pathname,
          )
        )
          requests.push(request.method());
      });
      await page.route("**/api/vault/setup", async (route) =>
        route.fulfill({
          json: {
            requestId: route.request().postDataJSON().requestId,
            state: "running",
            prefix: "project-one",
          },
        }),
      );
      await page.clock.install();
      await page.goto(fixtureOrigin(server) + `/check?flow=${flow}`);
      if (flow === "vault") {
        await page
          .getByRole("button", { name: "Show the paper key in a terminal" })
          .click();
      }
      if (flow === "vault")
        assert.equal(
          await page.getByLabel("Vault name", { exact: true }).isDisabled(),
          true,
          "keep the subscribed prefix fixed while generating its paper key",
        );
      if (flow === "vault-job") {
        await page
          .getByRole("button", { name: "Create vault", exact: true })
          .click();
        await page.waitForFunction(
          () =>
            window.transientProgressFixture.reads.jobs === 1 &&
            window.transientProgressFixture.selectors.some(
              (r) => r.kind === "vault-job-progress",
            ),
        );
      }
      const before = await page.evaluate(() => ({
        ...window.transientProgressFixture.reads,
      }));
      await page.clock.fastForward(10_000);
      assert.deepEqual(
        await page.evaluate(() => window.transientProgressFixture.reads),
        before,
      );
      assert.deepEqual(
        before,
        flow === "vault"
          ? { facts: 1, auth: 0, jobs: 0 }
          : flow === "vault-job"
            ? { facts: 1, auth: 0, jobs: 1 }
            : { facts: 0, auth: 0, jobs: 0 },
      );
      const kind =
        flow === "vault"
          ? "vault-readiness"
          : flow === "vault-job"
            ? "vault-job-progress"
            : "auth-progress";
      await page.waitForFunction(
        (kind) =>
          window.transientProgressFixture.selectors.some(
            (resource) => resource.kind === kind,
          ),
        kind,
      );
      await page.evaluate((flow) => {
        if (flow === "vault")
          window.transientProgressFixture.progress({
            resource: { kind: "vault-readiness", prefix: "project-one" },
            value: true,
          });
        else if (flow === "vault-job") {
          const resource = window.transientProgressFixture.selectors.find(
            (r) => r.kind === "vault-job-progress",
          );
          if (!resource) throw new Error("Missing vault progress subscription");
          window.transientProgressFixture.progress({
            resource,
            value: {
              requestId: resource.requestId,
              state: "running",
              prefix: "project-one",
              progress: "creating stack",
            },
          });
          window.transientProgressFixture.progress({
            resource,
            value: {
              requestId: resource.requestId,
              state: "done",
              prefix: "project-one",
            },
          });
        } else
          window.transientProgressFixture.progress({
            resource: { kind: "auth-progress", requestId: "login-one" },
            value: {
              requestId: "login-one",
              accountKey: "claude-one",
              status: "ready",
            },
          });
      }, flow);
      if (flow === "vault")
        await page.getByText("The paper key of project-one is made.").waitFor();
      else
        await page.waitForFunction(
          () => window.transientProgressFixture.ready === true,
        );
      if (flow === "vault-job")
        assert.deepEqual(
          requests,
          ["POST"],
          "only the explicit start POST occurs during the running job",
        );
      console.log(
        JSON.stringify({
          flow,
          idleSeconds: 10,
          idleReads: 0,
          initialReads: before,
        }),
      );
    } finally {
      await page.close();
      await server.close();
      await rm(cacheDir, { recursive: true, force: true });
    }
  });
}

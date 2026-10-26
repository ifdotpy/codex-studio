import type { PostBody } from "../../src/apiTypes.ts";
import assert from "node:assert/strict";
import { required, fixturePostBody } from "../teams/helpers.ts";
import { createFixtureViteServer as createServer } from "../playwright.ts";
// Exercise the Codex 0.159 write_stdin_approval request through the real card and answer API.
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "../playwright.ts";

test("Terminal Input Approval Ui", async ({
  browser: _testBrowser,
  context: _testContext,
  page: testPage,
}) => {
  const root = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const temporary = await mkdtemp(join(tmpdir(), "studio-stdin-approval-"));

  const server = await createServer({
    configFile: false,
    root: join(root, "workspaces/client/apps/web"),
    cacheDir: join(temporary, "vite"),
    plugins: [
      {
        name: "stdin-approval-fixture",
      },
    ],
    server: { host: "127.0.0.1", port: 0, hmr: false },
  });
  await server.listen();
  try {
    const page = testPage;
    page.setDefaultTimeout(5000);
    const answers: PostBody<"/api/answer">[] = [];
    await page.route("**/api/**", async (route) => {
      if (route.request().method() === "POST") {
        answers.push(fixturePostBody(route, "/api/answer"));
        return route.fulfill({ json: { status: "answered" } });
      }
      return route.fulfill({ json: {} });
    });
    await page.route("**/check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: '<!doctype html><div id="root"></div><script type="module">import "/tests/terminal/stdin-approval-fixture.tsx";</script>',
      }),
    );
    await page.goto(required(server.resolvedUrls?.local[0]) + "check");
    await page.waitForFunction(() =>
      document.querySelector('[data-request="request-159"]'),
    );
    await page
      .getByText("Terminal input approval required", { exact: true })
      .waitFor();
    await page
      .getByText(
        "Send input to an existing terminal to continue the reviewed command.",
        { exact: true },
      )
      .waitFor();
    await page
      .getByRole("button", { name: "Allow input", exact: true })
      .click();
    const deadline = Date.now() + 5000;
    while (!answers.length && Date.now() < deadline)
      await new Promise((resolve) => setTimeout(resolve, 10));
    assert.deepEqual(answers, [{ decision: "accept", id: "request-159" }]);
    console.log(
      "PASS: 0.159 writeStdin request renders its reason and submits one approval reply",
    );
  } finally {
    await server.close();
    await rm(temporary, { recursive: true, force: true });
  }
});

// Production renderer and isolated durable queue. No model service or user state.
import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { test, expect } from "../playwright.mjs";
import { spawnFixture as spawn } from "../playwright.mjs";

test("message-queue-ui", async ({ browser }) => {
  test.setTimeout(120_000);
  const repo = join(import.meta.dirname, "../../../");
  const root = await mkdtemp(join(tmpdir(), "studio-message-queue-ui-"));
  const models = ["gpt-6-astra", "gpt-5.6-sol", "gpt-5.6-luna"].map(
    (model) => ({
      model,
      defaultReasoningEffort: "medium",
      supportedReasoningEfforts: (model === "gpt-5.6-luna"
        ? ["low", "medium", "high", "max"]
        : ["low", "medium", "high", "ultra"]
      ).map((reasoningEffort) => ({ reasoningEffort })),
      serviceTiers: [{ id: "priority" }],
    }),
  );
  const proc = spawn(
    process.env.PYTHON || "python3",
    ["-B", join(repo, "tests/simple-ui-fixture.py"), root],
    {
      stdio: ["pipe", "pipe", "pipe"],
      env: {
        ...process.env,
        CODEX_BOARD_STATE_DIR: join(root, "board"),
        EXECUTION_SETTINGS_CATALOG: JSON.stringify(models),
      },
    },
  );
  let context,
    page,
    log = "";
  const diagnostics = [];
  proc.stderr.on("data", (data) => {
    log += data;
  });
  try {
    const port = await new Promise((resolve, reject) => {
      const timer = setTimeout(
        () => reject(Error("Fixture startup timed out: " + log)),
        30000,
      );
      proc.stdout.once("data", (data) => {
        clearTimeout(timer);
        resolve(Number(String(data).trim()));
      });
      proc.once("exit", () => {
        clearTimeout(timer);
        reject(Error(log || "Fixture exited"));
      });
    });
    const origin = `http://127.0.0.1:${port}`;
    const initial = await (await fetch(origin + "/api/state")).json();
    const lead = initial.threads.find((agent) => agent.name === "Release lead");
    const queue = () =>
      fetch(`${origin}/api/queue?agent=${lead.id}`).then((response) =>
        response.json(),
      );
    const post = async (body) => {
      const response = await fetch(origin + "/api/queue", {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          Origin: origin,
          "X-Canvas-Token": initial.token,
        },
        body: JSON.stringify({
          agent: lead.id,
          request_id: randomUUID(),
          ...body,
        }),
      });
      assert.equal(response.status, 200, await response.clone().text());
      return response.json();
    };
    for (const item of (await queue()).items) {
      const current = await queue();
      await post({
        action: "cancel",
        message_id: item.id,
        expectedText: item.text,
        expected_revision: current.revision,
      });
    }
    const wait = async (test, label) => {
      for (let i = 0; i < 120; i++) {
        if (await test()) return;
        await new Promise((resolve) => setTimeout(resolve, 50));
      }
      throw Error(label + "\n" + log);
    };
    // Fault injection must observe fetches after reload, including in WebKit.
    context = await browser.newContext({
      viewport: { width: 1440, height: 960 },
      serviceWorkers: "block",
    });
    page = await context.newPage();
    page.setDefaultTimeout(10000);
    const errors = [],
      sent = [],
      mutations = [];
    page.on("pageerror", (error) => {
      errors.push(error.message);
      diagnostics.push({
        kind: "pageerror",
        message: error.message,
        stack: error.stack,
        at: Date.now(),
      });
    });
    page.on("requestfailed", (request) =>
      diagnostics.push({
        kind: "requestfailed",
        url: request.url(),
        failure: request.failure(),
        at: Date.now(),
      }),
    );
    page.on("request", (request) => {
      if (request.method() !== "POST") return;
      const path = new URL(request.url()).pathname;
      if (path === "/api/messages") sent.push(request.postDataJSON());
      if (path === "/api/queue") mutations.push(request.postDataJSON());
    });
    await page.addInitScript(
      ({ id, stateDir }) => {
        localStorage.setItem(
          `codex-desktop-opened:${stateDir}`,
          JSON.stringify(id),
        );
        localStorage.setItem("codex-mobile-opened", JSON.stringify(id));
      },
      { id: lead.id, stateDir: initial.stateDir },
    );
    await page.goto(origin);
    await page.locator(`[data-chat="${lead.id}"]`).click();
    const composer = page.locator("#message"),
      panel = page.getByTestId("message-queue"),
      list = page.getByRole("list", { name: "Queued messages", exact: true });
    const button = (name) => panel.getByRole("button", { name, exact: true });
    const editor = () =>
      panel.getByRole("textbox", {
        name: "Edit queued message",
        exact: true,
      });
    const attach = async (name) => {
      await page.getByLabel("Choose attachments").setInputFiles({
        name,
        mimeType: "text/plain",
        buffer: Buffer.from("Queue attachment fixture"),
      });
      await page
        .getByRole("button", { name: `Remove ${name}`, exact: true })
        .waitFor();
    };
    await composer.fill("First input");
    await attach("queued-evidence.txt");
    await composer.press("Tab");
    await wait(
      async () => (await queue()).items.length === 1,
      "First input reaches queue",
    );
    assert.equal(sent.length, 1);
    assert.equal(sent[0].delivery, "after_turn", "Tab sends after-turn input");
    await panel.waitFor();
    await list.getByText("First input", { exact: true }).waitFor();
    for (const width of [1440, 390]) {
      await page.setViewportSize({ width, height: 960 });
      for (const colorScheme of ["dark", "light"]) {
        await page.emulateMedia({ colorScheme });
        const size = await panel.evaluate((element) => {
          const panelBox = element.getBoundingClientRect();
          const contentBox = element
            .querySelector("ol")
            .getBoundingClientRect();
          const preview = element.querySelector(".message-queue-preview");
          return {
            gap: panelBox.bottom - contentBox.bottom,
            previewHeight: preview.getBoundingClientRect().height,
            previewText: preview.textContent,
          };
        });
        assert(
          size.gap >= 0 && size.gap <= 2,
          `Queue fits its content: ${JSON.stringify(size)}`,
        );
        assert(
          size.previewHeight <= 37,
          `Short text occupies one row: ${JSON.stringify(size)}`,
        );
        assert.equal(size.previewText, "First input");
        await page.screenshot({
          path: join(root, `queue-${width}-${colorScheme}.png`),
        });
      }
    }
    await page.setViewportSize({ width: 1440, height: 960 });
    const asset = (await queue()).items[0].assets[0].id;

    for (const text of ["Second input", "Third input"]) {
      await composer.fill(text);
      await composer.press("Tab");
      await wait(
        async () => (await queue()).items.some((item) => item.text === text),
        `${text} reaches queue`,
      );
    }
    await wait(
      async () => (await queue()).items.length === 3,
      "All inputs are durable",
    );
    assert.equal(sent.length, 3);
    assert(sent.every((entry) => entry.delivery === "after_turn"));
    await button("Edit queued message 1").click();
    await editor().fill("Revised first input");
    await button("Save queued message").click();
    await wait(
      async () => (await queue()).items[0].text === "Revised first input",
      "Edit persists",
    );
    assert.equal((await queue()).items[0].assets[0].id, asset);
    await button("Move queued message 3 up").click();
    await wait(
      async () => (await queue()).items[1].text === "Third input",
      "Reorder persists",
    );
    const order = (await queue()).items.map((item) => item.id);
    await page.reload();
    await panel.waitFor();
    assert.deepEqual(
      (await queue()).items.map((item) => item.id),
      order,
    );
    await list.getByText("Revised first input", { exact: true }).waitFor();
    const longText =
      "A long queued message with details that span several lines. ".repeat(9);
    await composer.fill(longText);
    await composer.press("Tab");
    await button("Expand queued message 4").waitFor();
    const shortHeight = await list.locator("li").nth(3).boundingBox();
    await button("Expand queued message 4").click();
    await button("Collapse queued message 4").waitFor();
    const fullHeight = await list.locator("li").nth(3).boundingBox();
    assert(fullHeight.height > shortHeight.height, "Long text expands");
    await button("Collapse queued message 4").click();
    const sendNowRequests = [];
    await page.route("**/api/queue", async (route) => {
      if (
        route.request().method() === "POST" &&
        route.request().postDataJSON()?.action === "send_now"
      ) {
        sendNowRequests.push(route.request().postDataJSON());
        if (sendNowRequests.length === 1) return route.abort("failed");
      }
      await route.continue();
    });
    await button("Send queued message 1 now").click();
    await panel.getByRole("alert").waitFor();
    assert.equal(sendNowRequests.length, 1);
    await page.getByRole("button", { name: "Retry queue change" }).click();
    await wait(
      async () => sendNowRequests.length === 2,
      "Retry uses the saved send-now request",
    );
    assert.deepEqual(sendNowRequests[1], sendNowRequests[0]);
    await wait(
      async () => (await queue()).items[0]?.delivery === "steer",
      "Send now changes one queued input to steer",
    );
    await page.unrouteAll({ behavior: "wait" });
    expect(errors).toEqual([]);
    console.log(
      "PASS Tab after-turn queue, pending edit, asset, reorder and reload",
    );
  } catch (error) {
    console.error(error);
    console.error(JSON.stringify({ evidence: root, diagnostics }, null, 2));
    await page?.screenshot({ path: join(root, "failure.png") }).catch(() => {});
    throw error;
  } finally {
    await page?.unrouteAll({ behavior: "ignoreErrors" });
    await context?.close();
  }
});

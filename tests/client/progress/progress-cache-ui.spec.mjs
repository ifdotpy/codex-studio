#!/usr/bin/env node
// Production renderer, real progress files, isolated backend. No model calls.

import { mkdir, mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { test, expect, spawnFixture as spawn } from "../playwright.mjs";

test("Progress Cache Ui", async ({
  browser: testBrowser,
  context: _testContext,
  page: _testPage,
}) => {
  const assert = {
    equal: (actual, expected, message) =>
      expect(actual, message).toBe(expected),
    notEqual: (actual, expected, message) =>
      expect(actual, message).not.toBe(expected),
    deepEqual: (actual, expected, message) =>
      expect(actual, message).toEqual(expected),
    ok: (actual, message) => expect(actual, message).toBeTruthy(),
    match: (actual, expected, message) =>
      expect(actual, message).toMatch(expected),
    doesNotMatch: (actual, expected, message) =>
      expect(actual, message).not.toMatch(expected),
    fail: (message) => {
      throw new Error(message);
    },
  };

  const repo = join(import.meta.dirname, "../../..");
  const root = await mkdtemp(join(tmpdir(), "studio-progress-cache-ui-"));
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
        EXECUTION_SETTINGS_CATALOG: JSON.stringify(models),
        CODEX_BOARD_STATE_DIR: join(root, "board"),
      },
    },
  );
  let browser,
    context,
    page,
    log = "";
  proc.stderr.on("data", (data) => {
    log += data;
  });
  const diagnostics = [];
  const until = async (test, label) => {
    for (let i = 0; i < 160; i++) {
      if (await test()) return;
      await new Promise((resolve) => setTimeout(resolve, 75));
    }
    throw Error(label + "\n" + log);
  };
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
    const other = initial.threads.find(
      (agent) => agent.name === "Other project",
    );
    const readPanel = (agent) =>
      fetch(`${origin}/api/panel?agent=${agent.id}`).then((response) =>
        response.json(),
      );
    const files = new Map();
    for (const agent of [lead, other]) {
      const response = await readPanel(agent);
      assert.ok(
        response.path.startsWith(root + "/"),
        "Only task-owned progress files may change",
      );
      files.set(agent.id, response.path);
      await mkdir(dirname(response.path), { recursive: true });
    }
    // The fixture has 40 workers under Release lead. Other project must be a
    // nearest switch target to exercise the documented prefetch window.
    const pinTarget = await fetch(origin + "/api/organization", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-Canvas-Token": initial.token,
      },
      body: JSON.stringify({ id: other.id, pinned: true }),
    });
    assert.equal(pinTarget.ok, true, await pinTarget.text());
    const texts = {
      lead: "Lead cached progress.",
      other: "Other prefetched progress.",
      update: "Lead updated progress.",
    };
    await writeFile(files.get(lead.id), texts.lead);
    await writeFile(files.get(other.id), texts.other);
    browser = testBrowser;
    // Transport fault injection must remain observable after reload in WebKit.
    context = await browser.newContext({
      viewport: { width: 1440, height: 960 },
      serviceWorkers: "block",
    });
    page = await context.newPage();
    page.setDefaultTimeout(12000);
    const errors = [],
      responses = [],
      held = [];
    let hold = false;
    page.on("pageerror", (error) => {
      errors.push(error.message);
      diagnostics.push({ error: error.message, at: Date.now() });
    });
    page.on("response", async (response) => {
      const url = new URL(response.url());
      if (
        url.pathname !== "/api/panel" ||
        response.request().method() !== "GET"
      )
        return;
      try {
        responses.push(await response.json());
      } catch {
        /* Navigation can cancel an old response. */
      }
    });
    await page.route("**/api/panel?*", async (route) => {
      if (hold) {
        held.push(route);
        return;
      }
      await route.continue();
    });
    const release = async () => {
      hold = false;
      await Promise.all(
        held.splice(0).map((route) => route.continue().catch(() => {})),
      );
    };
    await context.addInitScript(
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
    await page.locator("#message").waitFor();
    await page.evaluate(() => document.fonts.ready);
    const panel = () =>
      page.getByRole("region", { name: "Agent progress", exact: true });
    const currentText = () =>
      panel().locator(".agent-panel-current, .agent-panel-saved");
    await currentText().getByText(texts.lead, { exact: true }).waitFor();
    await until(
      () =>
        responses.some(
          (response) =>
            response.agent === other.id && response.markdown === texts.other,
        ),
      "An unvisited chat prefetches its actual progress file",
    );
    assert.equal(
      await page
        .locator(`[data-chat="${lead.id}"]`)
        .getAttribute("aria-current"),
      "true",
    );
    console.log(
      "PASS unvisited chat progress loads in the existing background prefetch",
    );
    await page.evaluate(
      ({ lead, other }) => {
        window.progressFlashes = [];
        new MutationObserver(() => {
          const selected = document.querySelector(
            '[data-chat][aria-current="true"]',
          );
          const panel = document.querySelector(".agent-panel");
          const text = panel?.querySelector(
            ".agent-panel-current, .agent-panel-saved",
          )?.textContent;
          if (!selected || !panel || !text) return;
          const id = selected.getAttribute("data-chat");
          const correct =
            panel.getAttribute("data-agent") === id &&
            (id === lead
              ? text.startsWith("Lead ")
              : id === other
                ? text.startsWith("Other ")
                : true);
          if (!correct)
            window.progressFlashes.push({
              selected: id,
              agent: panel.getAttribute("data-agent"),
              text,
            });
        }).observe(document.body, {
          childList: true,
          subtree: true,
          attributes: true,
        });
      },
      { lead: lead.id, other: other.id },
    );

    // Observe the first animation frame after the real sidebar handler runs.
    // No response can supply the expected text during this interval.
    const switchFrame = async (agent, expected) => {
      const frame = await page.evaluate(
        ({ id }) => {
          const started = performance.now();
          document.querySelector(`[data-chat="${id}"]`).click();
          return new Promise((resolve) =>
            requestAnimationFrame(() => {
              const node = document.querySelector(".agent-panel");
              const content = node?.querySelector(
                ".agent-panel-current, .agent-panel-saved",
              );
              resolve({
                agent: node?.getAttribute("data-agent"),
                text: content?.textContent,
                fit: node?.getAttribute("data-fit"),
                cached: node?.getAttribute("data-cached"),
                height: content?.getBoundingClientRect().height,
                elapsed: performance.now() - started,
              });
            }),
          );
        },
        { id: agent.id },
      );
      assert.equal(
        frame.agent,
        agent.id,
        "First frame belongs to the selected chat",
      );
      assert.equal(
        frame.text,
        expected,
        "Cached progress is visible in the first animation frame",
      );
      assert.equal(frame.fit, "yes");
      assert.equal(
        frame.cached,
        "yes",
        "Held refresh shows an explicit saved copy",
      );
      assert.ok(frame.height > 0);
      diagnostics.push({ switch: agent.id, ...frame });
    };
    hold = true;
    await switchFrame(other, texts.other);
    await switchFrame(lead, texts.lead);
    await until(
      () => held.length > 0,
      "Foreground refresh waits on the held network",
    );
    assert.equal(await currentText().textContent(), texts.lead);
    console.log(
      "PASS prefetched and visited progress render in the first frame with panel HTTP held",
    );

    await writeFile(files.get(lead.id), texts.update);
    const updated = await readPanel(lead);
    await switchFrame(other, texts.other);
    await release();
    await until(
      () =>
        responses.some(
          (response) =>
            response.agent === other.id && response.markdown === texts.other,
        ),
      "Other progress remains available",
    );
    await until(
      () =>
        responses.some(
          (response) =>
            response.agent === lead.id &&
            response.revision === updated.revision,
        ),
      "The background refresh reads the changed file before switching back",
    );
    assert.equal(await panel().getAttribute("data-agent"), other.id);
    assert.equal(
      await currentText().textContent(),
      texts.other,
      "An old chat response cannot replace the current chat",
    );
    await page.locator(`[data-chat="${lead.id}"]`).click();
    await panel()
      .locator(".agent-panel-current")
      .getByText(texts.update, { exact: true })
      .waitFor();
    assert.equal(
      await panel().getAttribute("data-panel-revision"),
      updated.revision,
    );
    assert.equal(await panel().getAttribute("data-cached"), "no");
    console.log("PASS background revision refresh preserves chat isolation");

    assert.deepEqual(
      await page.evaluate(() => window.progressFlashes || []),
      [],
      "No visible progress belongs to another chat",
    );
    hold = true;
    await page.reload();
    await currentText().getByText(texts.update, { exact: true }).waitFor();
    assert.equal(
      await panel().getAttribute("data-panel-revision"),
      updated.revision,
    );
    assert.equal(await panel().getAttribute("data-fit"), "yes");
    await context.setOffline(true);
    await switchFrame(other, texts.other);
    await switchFrame(lead, texts.update);
    await page.screenshot({
      path: join(root, "progress-cached-offline-desktop.png"),
      animations: "disabled",
    });
    console.log(
      "PASS reload restores durable progress; offline chat switches retain measured content",
    );

    await page.setViewportSize({ width: 390, height: 844 });
    await panel().getByRole("button", { name: "Show", exact: true }).click();
    await currentText().getByText(texts.update, { exact: true }).waitFor();
    assert.equal(await panel().getAttribute("data-fit"), "yes");
    const phoneGeometry = await panel().evaluate((node) => ({
      width: node.clientWidth,
      scrollWidth: node.scrollWidth,
      height: node.getBoundingClientRect().height,
    }));
    assert.ok(
      phoneGeometry.scrollWidth <= phoneGeometry.width + 1 &&
        phoneGeometry.height <= 237,
      JSON.stringify(phoneGeometry),
    );
    assert.ok(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth + 1,
      ),
    );
    await page.screenshot({
      path: join(root, "progress-cached-offline-mobile.png"),
      animations: "disabled",
    });
    await context.setOffline(false);
    await release();
    await writeFile(
      files.get(lead.id),
      Array.from({ length: 80 }, (_, index) => `Lead paragraph ${index}.`).join(
        "\n\n",
      ),
    );
    await panel()
      .getByRole("button", { name: "Expand", exact: true })
      .waitFor();
    assert.equal(await panel().getAttribute("data-fit"), "no");
    assert.equal(
      await currentText().count(),
      1,
      "Overflow remains visible with clipping",
    );
    assert.ok((await panel().boundingBox()).height <= 237);
    assert.ok((await page.locator("#messages").boundingBox()).height > 100);
    assert.ok((await page.locator("#composer").boundingBox()).height > 80);
    for (const width of [1440, 390]) {
      await page.setViewportSize({ width, height: 900 });
      await panel().getByRole("button", { name: "Hide", exact: true }).click();
      assert.ok(
        (await panel().boundingBox()).height <= (width === 390 ? 45 : 33),
      );
      await page.screenshot({
        path: join(root, `progress-hidden-${width}.png`),
        animations: "disabled",
      });
      await panel().getByRole("button", { name: "Show", exact: true }).click();
      const header = await panel()
        .locator(".agent-panel-heading")
        .boundingBox();
      const hide = await panel()
        .getByRole("button", { name: "Hide", exact: true })
        .boundingBox();
      const expand = await panel()
        .getByRole("button", { name: "Expand", exact: true })
        .boundingBox();
      assert.ok(
        Math.abs(expand.x + expand.width - (header.x + header.width - 10)) <=
          0.5,
        "Expand aligns with the right header padding",
      );
      assert.ok(
        Math.abs(expand.x - (hide.x + hide.width) - 8) <= 0.5 &&
          hide.y === expand.y,
        "Hide and Expand are adjacent on the right, in that order",
      );
      if (width === 390) {
        for (const control of [hide, expand])
          assert.ok(control.width >= 44 && control.height >= 44);
      }
      await page.screenshot({
        path: join(root, `progress-preview-${width}.png`),
        animations: "disabled",
      });
    }
    if (process.env.PROGRESS_SCREENSHOTS) {
      for (const width of [1440, 390]) {
        await page.setViewportSize({ width, height: 900 });
        await page.waitForTimeout(150);
        await page.screenshot({
          path: `docs/verification/progress-fit/app-${process.env.PROGRESS_SCREENSHOTS}-${width}.png`,
        });
        await panel()
          .getByRole("button", { name: "Expand", exact: true })
          .click();
        await page.screenshot({
          path: `docs/verification/progress-fit/app-expanded-${width}.png`,
        });
        await panel()
          .getByRole("button", { name: "Collapse", exact: true })
          .click();
      }
    }
    hold = true;
    await page.reload();
    await panel().locator(".agent-panel-saved").waitFor();
    await context.setOffline(true);
    assert.equal(await panel().getAttribute("data-clipped"), "yes");
    assert.equal(await panel().getAttribute("data-cached"), "yes");
    await panel().getByRole("button", { name: "Expand", exact: true }).click();
    assert.equal(await panel().getAttribute("data-expanded"), "yes");
    await panel()
      .getByRole("button", { name: "Collapse", exact: true })
      .click();
    await context.setOffline(false);
    await release();
    await writeFile(files.get(lead.id), "");
    await panel().waitFor({ state: "hidden" });
    assert.deepEqual(
      await page.evaluate(() => window.progressFlashes || []),
      [],
      "No visible progress belongs to another chat",
    );
    hold = true;
    await page.reload();
    await page.locator("#message").waitFor();
    assert.equal(
      await panel().count(),
      0,
      "A cached empty file cannot resurrect old progress",
    );
    console.log(
      "PASS cached mobile content fits; clipped overflow and empty revisions preserve the current content",
    );
    assert.deepEqual(
      await page.evaluate(() => window.progressFlashes || []),
      [],
      "No visible progress belongs to another chat",
    );
    assert.deepEqual(errors, []);
    console.log(
      JSON.stringify({
        browser: browser.browserType().name(),
        evidence: root,
        firstFrames: diagnostics.filter((item) => item.switch),
        panelResponses: responses.length,
      }),
    );
  } catch (error) {
    console.error(error);
    console.error(JSON.stringify({ evidence: root, diagnostics }, null, 2));
    await page?.screenshot({ path: join(root, "failure.png") }).catch(() => {});
    throw error;
  } finally {
    await context?.close();
  }
});

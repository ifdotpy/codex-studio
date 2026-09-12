#!/usr/bin/env node
// Production transcript and result renderers with isolated saved messages. No inference.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";

const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium, webkit } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const engine = process.env.BROWSER === "webkit" ? webkit : chromium;
const mobile = engine === webkit || process.env.MOBILE === "1";
const directory = await mkdtemp(join(tmpdir(), "studio-file-change-ui-"));
const fixture = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), directory],
  {
    stdio: ["ignore", "pipe", "pipe"],
  },
);
let browser,
  page,
  log = "";
fixture.stderr.on("data", (data) => {
  log += data;
});
const update =
  "--- a/src/example.ts\n+++ b/src/example.ts\n@@ -1,2 +1,2 @@\n const keep = true;\n-const answer = 41;\n+const answer = 42;\n";
const longPath = `src/${"nested-segment-".repeat(12)}/long-file.ts`;
const longContent =
  Array.from(
    { length: 45 },
    (_, i) => `const value${i + 1} = "${i === 0 ? "x".repeat(450) : "saved"}";`,
  ).join("\n") + "\n";
const unsafe = '<img src=x onerror="window.diffExecuted=true">';
const change = (path, type, diff, extra = {}) => ({
  path,
  kind: { type, ...extra },
  diff,
});
const patch = (id, changes, options = {}) => {
  const { status = "completed", error, truncated = false } = options;
  return {
    id,
    role: "output",
    title: "File changes",
    turnId: "file-turn",
    toolStatus: status === "inProgress" ? "running" : status,
    truncated,
    text: JSON.stringify({
      id,
      type: "fileChange",
      status,
      changes,
      ...(error ? { error } : {}),
    }),
  };
};
const items = [
  {
    id: "ask",
    role: "user",
    turnId: "file-turn",
    text: "Update the fixture files.",
  },
  patch("edit", [change("src/example.ts", "update", update)]),
  patch("raw", [
    change("added.txt", "add", "+literal plus\n-literal minus\n"),
    change("deleted.txt", "delete", "-old marker\n+old marker\n"),
  ]),
  patch("rename", [
    change("old-name.ts", "update", "@@ -4,1 +4,1 @@\n-before\n+after\n", {
      move_path: "new-name.ts",
    }),
  ]),
  patch("hunks", [
    change(
      "multi.ts",
      "update",
      "@@ -1,2 +1,2 @@\n-old\n+new\n same\n@@ -20,1 +20,2 @@\n-again\n+next\n+extra\n",
    ),
  ]),
  patch("long", [change(longPath, "add", longContent)]),
  patch("unsafe", [change("literal.html", "add", unsafe + "\n")]),
  patch("failed", [change("failed.ts", "update", update)], {
    status: "failed",
    error: { message: "Patch context did not match" },
  }),
  patch("declined", [change("declined.ts", "update", update)], {
    status: "declined",
  }),
  patch(
    "clipped",
    [change("clipped.ts", "update", "@@ -1,2 +1,2 @@\n-old\n+new\n")],
    { truncated: true },
  ),
  patch("unparsed", [
    change(
      "unparsed.ts",
      "update",
      "The native record has no unified hunk header.\n",
    ),
  ]),
  {
    id: "final",
    role: "assistant",
    turnId: "file-turn",
    phase: "final_answer",
    text: "The saved patch fixtures are ready.",
  },
];

try {
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (data) => resolve(Number(String(data).trim())));
    fixture.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const state = await (await fetch(origin + "/api/state")).json();
  const lead = state.threads.find((agent) => agent.name === "Other project");
  const transcript = {
    items,
    order: items.map((item) => item.id),
    replace: true,
    agent: { ...lead, status: "completed", inFlight: false, turnId: null },
  };
  browser = await engine.launch({
    headless: true,
    ...(engine === chromium
      ? {
          executablePath:
            process.env.CHROME_BIN ||
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        }
      : {}),
  });
  page = await browser.newPage({
    viewport: mobile
      ? { width: 390, height: 844 }
      : { width: 1280, height: 960 },
    ...(mobile ? { isMobile: true, hasTouch: true } : {}),
  });
  page.setDefaultTimeout(10000);
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.addInitScript(
    ({ id, stateDir }) => {
      localStorage.setItem("codex-mobile-opened", JSON.stringify(id));
      localStorage.setItem(
        `codex-desktop-opened:${stateDir}`,
        JSON.stringify(id),
      );
      window.EventSource = class extends EventTarget {
        close() {}
      };
    },
    { id: lead.id, stateDir: state.stateDir },
  );
  await page.route("**/api/sync/**", (route) =>
    route.fulfill({ status: 404, json: { error: "HTTP fixture" } }),
  );
  await page.route("**/api/transcript?*", (route) =>
    new URL(route.request().url()).searchParams.get("id") === lead.id
      ? route.fulfill({ json: transcript })
      : route.fallback(),
  );
  await page.goto(origin);
  const card = (id) =>
    page.locator(`details.file-change-card[data-message="${id}"]`);
  await card("edit").waitFor();
  assert.equal(
    await page.locator(".turn-work .file-change-card").count(),
    0,
    "file changes remain outside the work disclosure",
  );
  assert.equal(await page.locator(".file-change-card").count(), 10);
  assert.notEqual(
    await card("edit").getAttribute("open"),
    null,
    "saved changes start expanded",
  );
  const header = card("edit").locator(":scope > summary");
  assert.match(await header.innerText(), /Edited\s+src\/example\.ts/);
  assert.match(
    await header.locator(".file-diff-counts").innerText(),
    /\+1\s+-1/,
  );
  const rows = async (scope) =>
    scope.locator(".file-diff-line").evaluateAll((elements) =>
      elements.map((element) => ({
        kind: element.dataset.kind,
        numbers: [...element.querySelectorAll(".file-diff-number")]
          .map((node) => node.textContent.trim())
          .filter(Boolean),
        text: element.querySelector(".file-diff-text")?.textContent,
        color: getComputedStyle(element).color,
        background: getComputedStyle(element).backgroundColor,
      })),
    );
  const edited = await rows(card("edit"));
  const removed = edited.find((row) => row.kind === "delete");
  const added = edited.find((row) => row.kind === "add");
  assert.equal(removed.text, "const answer = 41;");
  assert.equal(added.text, "const answer = 42;");
  assert.ok(removed.numbers.includes("2"));
  assert.ok(added.numbers.includes("2"));
  assert.ok(edited.find((row) => row.kind === "context").numbers.includes("1"));
  assert.notEqual(
    JSON.stringify([removed.color, removed.background]),
    JSON.stringify([added.color, added.background]),
    "add and delete colors differ",
  );
  await header.click();
  assert.equal(await card("edit").getAttribute("open"), null);
  assert.equal(await header.isVisible(), true);
  assert.match(await header.innerText(), /Edited\s+src\/example\.ts/);
  assert.match(
    await header.locator(".file-diff-counts").innerText(),
    /\+1\s+-1/,
  );
  await header.click();

  assert.equal(await card("raw").locator(".file-diff-path").count(), 2);
  const raw = await rows(card("raw"));
  assert.deepEqual(
    raw.filter((row) => row.kind === "add").map((row) => row.text),
    ["+literal plus", "-literal minus"],
  );
  assert.deepEqual(
    raw.filter((row) => row.kind === "delete").map((row) => row.text),
    ["-old marker", "+old marker"],
  );
  assert.match(await card("rename").innerText(), /old-name\.ts/);
  assert.match(await card("rename").innerText(), /new-name\.ts/);
  assert.match(
    await card("hunks")
      .locator(":scope > summary .file-diff-counts")
      .innerText(),
    /\+3\s+-2/,
  );
  assert.ok(
    (await rows(card("hunks")))
      .find((row) => row.text === "extra")
      .numbers.includes("21"),
  );

  assert.equal(await card("long").locator(".file-diff-line").count(), 20);
  await card("long")
    .getByRole("button", { name: "Show all 45 lines", exact: true })
    .click();
  assert.equal(await card("long").locator(".file-diff-line").count(), 45);
  await card("long")
    .getByRole("button", { name: "Show fewer lines", exact: true })
    .click();
  assert.equal(await card("long").locator(".file-diff-line").count(), 20);
  const wrapping = await card("long").evaluate((element) => ({
    width: element.clientWidth,
    scroll: element.scrollWidth,
  }));
  assert.ok(
    wrapping.scroll <= wrapping.width + 1,
    `long path and content fit: ${JSON.stringify(wrapping)}`,
  );
  assert.equal(
    (await rows(card("unsafe"))).find((row) => row.kind === "add").text,
    unsafe,
  );
  assert.equal(await card("unsafe").locator("img").count(), 0);
  assert.equal(await page.evaluate(() => !!window.diffExecuted), false);
  assert.equal(await card("failed").getAttribute("data-tool-status"), "failed");
  assert.doesNotMatch(
    await card("failed").locator(":scope > summary").innerText(),
    /Edited/,
  );
  assert.match(await card("failed").innerText(), /Patch context did not match/);
  assert.match(
    await card("declined").locator(":scope > summary").innerText(),
    /Patch declined/,
  );
  assert.doesNotMatch(
    await card("declined").locator(":scope > summary").innerText(),
    /Edited/,
  );
  assert.match(await card("clipped").innerText(), /clipped|truncated/i);
  assert.equal(
    await card("clipped").locator(".file-diff-counts").count(),
    0,
    "clipped content cannot claim complete counts",
  );
  assert.equal(
    await card("unparsed").locator(".file-diff-counts").count(),
    0,
    "unknown content cannot claim parsed counts",
  );
  assert.match(await card("unparsed").innerText(), /could not be parsed/);
  assert.equal(
    await card("unparsed").locator(".file-diff-raw").first().innerText(),
    "The native record has no unified hunk header.\n",
  );
  const source = card("edit").locator(".file-change-source");
  await source.locator(":scope > summary").click();
  assert.match(await source.innerText(), /Raw event/);
  assert.equal(
    JSON.parse(await source.locator("pre").innerText()).changes[0].diff,
    update,
  );
  await source.locator(":scope > summary").click();

  await page
    .locator(".conversation-result")
    .filter({ hasText: "example.ts" })
    .click();
  const modal = page.getByRole("dialog");
  await modal.locator(".file-diff").waitFor();
  const resultRows = await rows(modal);
  assert.equal(resultRows.find((row) => row.kind === "add").text, added.text);
  assert.equal(
    resultRows.find((row) => row.kind === "delete").text,
    removed.text,
  );
  await page.screenshot({ path: join(directory, "result.png") });
  await page.keyboard.press("Escape");
  await modal.waitFor({ state: "hidden" });
  const documentWidth = await page.evaluate(() => ({
    viewport: innerWidth,
    scroll: document.documentElement.scrollWidth,
  }));
  assert.ok(
    documentWidth.scroll <= documentWidth.viewport + 1,
    `no horizontal document overflow: ${JSON.stringify(documentWidth)}`,
  );
  await card("edit").scrollIntoViewIfNeeded();
  await page.screenshot({ path: join(directory, "transcript.png") });
  assert.deepEqual(errors, []);
  const result = {
    status: "PASS",
    engine: engine.name(),
    mobile,
    evidence: directory,
    checks: [
      "standalone diff",
      "line numbers and colors",
      "raw add/delete",
      "rename",
      "multiple hunks",
      "collapse",
      "bounded expansion",
      "long content wrap",
      "literal source",
      "failed and declined",
      "clipped source",
      "result modal",
    ],
  };
  await writeFile(
    join(directory, "result.json"),
    JSON.stringify(result, null, 2) + "\n",
  );
  console.log(JSON.stringify(result));
} catch (error) {
  await page
    ?.screenshot({ path: join(directory, "failure.png") })
    .catch(() => {});
  console.error("Evidence:", directory);
  throw error;
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
}

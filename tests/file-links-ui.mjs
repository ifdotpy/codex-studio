#!/usr/bin/env node
// Markdown links through the real file API and isolated SQLite. No model requests.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { createRequire } from "node:module";
const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const root = await mkdtemp(join(tmpdir(), "codex-file-links-ui-"));
const outsideRoot = await mkdtemp(join(tmpdir(), "codex-file-links-outside-"));
const outside = join(outsideRoot, "outside-project.md");
await writeFile(outside, "Outside project content");
const report = join(root, "official-build.md");
await writeFile(
  report,
  Array.from({ length: 400 }, (_, i) => `Report line ${i + 1}`).join("\n"),
);
await writeFile(join(root, "report with spaces.md"), "Encoded file content");
await writeFile(
  join(root, "report\u200bname.md"),
  "Interior character preserved",
);
const proc = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), root],
  { stdio: ["pipe", "pipe", "pipe"] },
);
let log = "",
  browser,
  page;
proc.stderr.on("data", (d) => (log += d));
const poll = async (fn) => {
  for (let i = 0; i < 150; i++) {
    if (await fn()) return;
    await new Promise((r) => setTimeout(r, 100));
  }
  throw Error("Fixture timeout " + log);
};
try {
  const port = await new Promise((resolve, reject) => {
    proc.stdout.once("data", (d) => resolve(Number(String(d).trim())));
    proc.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  browser = await chromium.launch({
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    headless: true,
  });
  page = await browser.newPage({ viewport: { width: 1280, height: 960 } });
  const errors = [],
    requests = [];
  page.on("pageerror", (e) => errors.push(e.message));
  page.on("request", (r) => {
    if (r.url().includes("/api/file?")) requests.push(new URL(r.url()));
  });
  await page.goto(origin);
  await page
    .locator("[data-chat]")
    .filter({ hasText: "Other project" })
    .click();
  await page.locator("#message").fill("File link regression");
  await page.locator("#send").click();
  let agent;
  await poll(async () => {
    agent = (
      await (await fetch(origin + "/api/state")).json()
    ).runtime.agents.find((a) => a.name === "Other project");
    return agent.status === "running" && agent.turnId;
  });
  const links = [
    `[отчёте official-build](${report}:357)`,
    `[Zero width prefix](\u200b${report}:383)`,
    `[Encoded zero width](%E2%80%8B${report}:384%E2%80%8B)`,
    "[Interior character](report%E2%80%8Bname.md)",
    "[Relative](official-build.md:3:2)",
    "[Anchor](./official-build.md#L25)",
    `[File URL](${pathToFileURL(report)}#L12)`,
    "[Encoded](report%20with%20spaces.md)",
    "[Missing](missing.md)",
    `[Outside](${outside})`,
    `[Zero width outside](\u200b${outside})`,
    "[Sandbox](sandbox:/mnt/data/report.md)",
    "[Remote file](file://remote.invalid/report.md)",
    "[External](https://example.com/report)",
    "[Unsafe](javascript:alert(1))",
  ].join("\n\n");
  proc.stdin.write(
    JSON.stringify({
      method: "item/completed",
      params: {
        threadId: agent.threadId,
        turnId: agent.turnId,
        item: { id: "file-links-report", type: "agentMessage", text: links },
      },
    }) + "\n",
  );
  const check = async (name, expected, line) => {
    await page.getByRole("link", { name, exact: true }).click();
    const dialog = page.getByRole("dialog");
    await dialog.locator(".workspace-file-text").waitFor();
    assert.match(
      await dialog.locator(".workspace-file-text").textContent(),
      expected,
    );
    if (line) {
      const marked = dialog.locator(`[data-file-line="${line}"]`);
      await marked.waitFor();
      assert.equal((await marked.textContent()).trim(), `Report line ${line}`);
      await poll(() =>
        marked.evaluate((el) => {
          const p = el.closest("pre").getBoundingClientRect(),
            r = el.getBoundingClientRect();
          return r.top >= p.top && r.bottom <= p.bottom;
        }),
      );
    }
    assert.equal(requests.at(-1).searchParams.get("agent"), agent.id);
    assert.equal(requests.at(-1).searchParams.has("line"), false);
    if (line === 357)
      await page.screenshot({ path: join(root, "official-build-preview.png") });
    await page.keyboard.press("Escape");
    await dialog.waitFor({ state: "hidden" });
    assert.equal(page.url(), origin + "/");
  };
  await check("отчёте official-build", /Report line 357/, 357);
  assert.equal(requests.at(-1).searchParams.get("path"), report);
  await check("Zero width prefix", /Report line 383/, 383);
  assert.equal(requests.at(-1).searchParams.get("path"), report);
  await check("Encoded zero width", /Report line 384/, 384);
  assert.equal(requests.at(-1).searchParams.get("path"), report);
  await check("Interior character", /Interior character preserved/);
  assert.equal(requests.at(-1).searchParams.get("path"), "report\u200bname.md");
  await check("Relative", /Report line 3/, 3);
  await check("Anchor", /Report line 25/, 25);
  await check("File URL", /Report line 12/, 12);
  await check("Encoded", /Encoded file content/);
  await check("Outside", /Outside project content/);
  assert.equal(requests.at(-1).searchParams.get("path"), outside);
  await check("Zero width outside", /Outside project content/);
  assert.equal(requests.at(-1).searchParams.get("path"), outside);
  for (const [name, message] of [
    ["Missing", /does not exist/],
    ["Sandbox", /does not exist/],
    ["Remote file", /Remote file links are not supported/],
  ]) {
    await page.getByRole("link", { name, exact: true }).click();
    await poll(async () =>
      message.test(
        await page.getByRole("dialog").getByRole("alert").textContent(),
      ),
    );
    await page.keyboard.press("Escape");
    await page.getByRole("dialog").waitFor({ state: "hidden" });
  }
  assert.equal(
    await page
      .getByRole("link", { name: "External", exact: true })
      .getAttribute("href"),
    "https://example.com/report",
  );
  assert.equal(
    await page.locator('#messages [href^="javascript:"]').count(),
    0,
  );
  assert.deepEqual(errors, []);
  console.log(
    "File links UI: PASS (official-build absolute line, boundary zero-width cleanup, encoded zero-width cleanup, interior character preserved, relative, anchors, file URL, encoded name, outside project preview, visible missing file errors, sandbox, remote file rejection, external preservation, unsafe URL sanitation)",
  );
  console.log("Evidence:", root);
} catch (error) {
  await page?.screenshot({ path: join(root, "failure.png") });
  console.error("Evidence:", root);
  throw error;
} finally {
  await browser?.close();
  proc.kill("SIGTERM");
  if (proc.exitCode === null) await new Promise((r) => proc.once("exit", r));
}

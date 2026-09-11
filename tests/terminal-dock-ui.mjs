#!/usr/bin/env node
// Terminal UI transport fixture; Python terminal contracts exercise actual PTYs.
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { readFile, mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, extname } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const skill = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(skill, "web/package.json"))(
  "playwright-core",
);
const root = await mkdtemp(join(tmpdir(), "codex-terminal-dock-"));
const server = createServer(async (req, res) => {
  try {
    const name = new URL(req.url, "http://localhost").pathname;
    const path = join(skill, "web/dist", name === "/" ? "index.html" : name);
    res.setHeader(
      "Content-Type",
      { ".js": "text/javascript", ".css": "text/css", ".html": "text/html" }[
        extname(path)
      ] || "application/octet-stream",
    );
    res.end(await readFile(path));
  } catch {
    res.statusCode = 404;
    res.end();
  }
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const agent = {
  id: "lead",
  rootId: "lead",
  isLead: true,
  source: "managed",
  status: "idle",
  name: "Release lead",
  model: "gpt-5.6-sol",
  created: Date.now() / 1000,
  canSend: true,
  cwd: "/workspace/release",
};
const command = {
  id: "native-command",
  agent: "lead",
  kind: "command",
  name: "commandExecution",
  command: "npm run dev",
  status: "running",
  created: Date.now() / 1000,
  tail: "Listening on localhost:3000",
};
const monitor = {
  id: "agent-monitor",
  agent: "lead",
  kind: "monitor",
  command: "await deployment",
  status: "running",
  interactive: false,
  created: Date.now() / 1000,
  tail: "Deployment pending",
};
const state = {
  token: "terminal-fixture-token",
  stateDir: root,
  threads: [agent],
  chats: [],
  runtime: {
    agents: [agent],
    rooms: [],
    complaints: [],
    monitors: [monitor],
    tasks: [command],
    requests: [],
  },
};
const shells = Array.from({ length: 250 }, (_, index) => ({
  id: `shell-${index}`,
  agent: "lead",
  title: `Shell ${String(index).padStart(3, "0")}`,
  cwd: "/workspace/release",
  status: index % 3 ? "running" : "exited",
  exitCode: index % 3 ? undefined : 0,
  created: Date.now() / 1000,
}));
const outputs = new Map(
  shells.map((shell) => [shell.id, `Welcome to ${shell.title}\r\n$ `]),
);
const writes = [];
let uncertainInput = false;
let failOutput = false,
  truncateOutput = false;
const archive = "Archive line λ\n".repeat(10000);
const archiveReads = [];
let historyMode = "ready",
  heldHistory;
let browser;
try {
  browser = await chromium.launch({
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    headless: true,
  });
  const page = await browser.newPage({
    viewport: { width: 1440, height: 980 },
    permissions: ["clipboard-read", "clipboard-write"],
  });
  const errors = [];
  page.on("pageerror", (error) => {
    errors.push(error.message);
    console.error(error.stack);
  });
  await page.route("**/api/**", async (route) => {
    const request = route.request(),
      url = new URL(request.url()),
      path = url.pathname;
    let body = {};
    if (request.method() === "POST") {
      body = request.postDataJSON();
      assert.equal(request.headers()["x-canvas-token"], state.token);
      writes.push({ path, body });
    }
    let value = {};
    if (path.startsWith("/api/sync/"))
      return route.fulfill({
        status: 404,
        json: { error: "No replication in this transport fixture" },
      });
    if (path === "/api/accounts")
      value = { accounts: [], defaultAccountKey: "default" };
    else if (path === "/api/voice/records")
      value = { records: [], cursor: 0, delivered: [] };
    else if (path === "/api/session") value = { token: state.token };
    else if (path === "/api/state") value = state;
    else if (path === "/api/transcript") value = { items: [], agent };
    else if (path === "/api/transcript/stream")
      return route.fulfill({ status: 503, body: "fixture polling" });
    else if (path === "/api/limits") value = { data: null };
    else if (path === "/api/terminals") value = { items: shells };
    else if (path === "/api/terminals/create") {
      value = {
        id: body.id,
        agent: body.agent,
        title: "Terminal 251",
        cwd: agent.cwd,
        status: "running",
        created: Date.now() / 1000,
      };
      shells.unshift(value);
      outputs.set(value.id, "Direct shell ready\r\n$ ");
    } else if (path === "/api/terminals/output") {
      if (url.searchParams.get("history") === "1") {
        archiveReads.push(Number(url.searchParams.get("offset")));
        if (historyMode === "hold") {
          heldHistory = route;
          return;
        }
        const offset = Number(url.searchParams.get("offset"));
        const end =
          historyMode === "invalid"
            ? offset
            : Math.min(
                archive.length,
                offset + Number(url.searchParams.get("limit")),
              );
        return route.fulfill({
          json: {
            text: archive.slice(offset, end),
            offset: end,
            availableOffset: archive.length,
            hasMore: end < archive.length,
            historyStart: 0,
            truncated: false,
            status: "exited",
          },
        });
      }
      if (failOutput) {
        failOutput = false;
        return route.fulfill({
          status: 503,
          json: { error: "Terminal connection interrupted" },
        });
      }
      const id = url.searchParams.get("id"),
        output = outputs.get(id) || "",
        shell = shells.find((item) => item.id === id);
      value = {
        text: output.slice(Number(url.searchParams.get("offset") || 0)),
        offset: output.length,
        truncated: truncateOutput,
        status: shell?.status || "exited",
      };
      truncateOutput = false;
    } else if (path === "/api/terminals/input") {
      assert.ok(body.request_id);
      if (uncertainInput) {
        uncertainInput = false;
        return route.fulfill({
          json: {
            ok: false,
            delivery: "uncertain",
            error: "Input delivery uncertain",
          },
        });
      }
      outputs.set(body.id, (outputs.get(body.id) || "") + body.text);
    } else if (path === "/api/terminals/rename") {
      Object.assign(
        shells.find((item) => item.id === body.id),
        { title: body.title },
      );
    } else if (path === "/api/terminals/close") {
      Object.assign(
        shells.find((item) => item.id === body.id),
        { status: "closed" },
      );
    }
    await route.fulfill({ json: value });
  });
  await page.goto(`http://127.0.0.1:${server.address().port}`);
  assert.equal(await page.locator("#import-chat, #other-sessions").count(), 0);
  const dock = page.getByRole("region", { name: "Terminals", exact: true });
  await dock
    .locator(".terminal-dock-count")
    .filter({ hasText: "250" })
    .waitFor({ timeout: 10000 })
    .catch(async (error) => {
      await page.screenshot({ path: join(root, "terminal-start-failure.png") });
      console.error(await page.locator("body").innerText(), errors);
      throw error;
    });
  const closedGeometry = await page.evaluate(() => {
    const dock = document
      .querySelector(".terminal-dock")
      .getBoundingClientRect();
    const sidebar = document.querySelector("#sidebar").getBoundingClientRect();
    const workspace = document
      .querySelector(".workspace")
      .getBoundingClientRect();
    return {
      dock: dock.toJSON(),
      sidebar: sidebar.toJSON(),
      workspace: workspace.toJSON(),
      height: innerHeight,
    };
  });
  assert.equal(
    closedGeometry.workspace.bottom,
    closedGeometry.height,
    "Collapsed terminals reserve no chat height",
  );
  assert.equal(
    closedGeometry.sidebar.bottom,
    closedGeometry.dock.top,
    "Sidebar ends above the terminal bar",
  );
  assert.equal(
    closedGeometry.dock.right,
    closedGeometry.sidebar.right,
    "Collapsed terminal bar matches sidebar width",
  );
  assert.ok(
    await dock.evaluate((el) => el.scrollWidth <= el.clientWidth),
    "Collapsed controls fit with 250 terminals",
  );
  await page.screenshot({ path: join(root, "terminal-collapsed-sidebar.png") });
  await dock.getByRole("button", { name: "Show terminals" }).click();
  await page.waitForFunction(() => {
    const dock = document
      .querySelector(".terminal-dock")
      .getBoundingClientRect();
    const workspace = document
      .querySelector(".workspace")
      .getBoundingClientRect();
    return (
      Math.abs(workspace.bottom - dock.top) <= 1 && dock.width === innerWidth
    );
  });
  await dock
    .locator(".terminal-dock-count")
    .filter({ hasText: "250" })
    .waitFor();
  assert.ok(
    (await dock.locator(".terminal-session").count()) < 30,
    "Only visible session rows render",
  );
  await dock.getByLabel("Find terminal", { exact: true }).fill("Shell 249");
  await dock.getByRole("button", { name: /Shell 249 Your terminal/ }).click();
  await dock
    .getByText("Session ended · Output retained", { exact: true })
    .waitFor();
  // Output polls faster than the session list. Wait for the close control's
  // session record to report the exit before checking the completed-session path.
  await dock
    .locator(".terminal-detail-status")
    .filter({ hasText: /^exited/ })
    .waitFor();
  assert.equal(
    await dock
      .getByRole("button", { name: "Ctrl+C", exact: true })
      .isDisabled(),
    true,
  );
  await dock.getByLabel("Find terminal", { exact: true }).fill("");
  await dock.getByRole("button", { name: "New terminal", exact: true }).click();
  await dock
    .getByRole("button", { name: "Terminal 251", exact: true })
    .waitFor();
  await dock.locator(".xterm-helper-textarea").waitFor();
  const created = writes.find(
    (write) => write.path === "/api/terminals/create",
  );
  assert.equal(created.body.agent, "lead");
  await dock.locator(".xterm-helper-textarea").focus();
  await page.keyboard.type("printf terminal-ok");
  await page.keyboard.press("Enter");
  await page.waitForFunction(() =>
    document
      .querySelector(".xterm-accessibility-tree")
      ?.textContent.includes("terminal-ok"),
  );
  assert.equal(
    writes
      .filter((write) => write.path === "/api/terminals/input")
      .map((write) => write.body.text)
      .join(""),
    "printf terminal-ok\r",
  );
  let downloadCount = 0;
  page.on("download", () => downloadCount++);
  const downloadResult = page.waitForEvent("download");
  await dock
    .getByRole("button", { name: "Download saved output", exact: true })
    .click();
  const downloaded = await downloadResult;
  assert.equal(await readFile(await downloaded.path(), "utf8"), archive);
  assert.deepEqual(archiveReads, [0, 65536, 131072]);
  historyMode = "hold";
  await dock
    .getByRole("button", { name: "Download saved output", exact: true })
    .click();
  for (let i = 0; i < 100 && !heldHistory; i++)
    await new Promise((resolve) => setTimeout(resolve, 20));
  assert.ok(heldHistory);
  await dock
    .getByRole("button", { name: "Cancel download", exact: true })
    .click();
  await heldHistory.fulfill({
    json: {
      text: "late",
      offset: 4,
      availableOffset: 4,
      hasMore: false,
      historyStart: 0,
      truncated: false,
    },
  });
  await dock.getByText("Download cancelled.", { exact: true }).waitFor();
  historyMode = "invalid";
  await dock
    .getByRole("button", { name: "Download saved output", exact: true })
    .click();
  await dock
    .getByText("The saved output did not advance. Try the download again.", {
      exact: true,
    })
    .waitFor();
  assert.equal(
    downloadCount,
    1,
    "A cancelled or invalid history never downloads a partial file",
  );
  historyMode = "ready";
  const screen = await dock.locator(".xterm-screen").boundingBox();
  await page.mouse.move(screen.x + 1, screen.y + 7);
  await page.mouse.down();
  await page.mouse.move(screen.x + 128, screen.y + 7, { steps: 8 });
  await page.mouse.up();
  await dock
    .getByRole("button", { name: "Copy selection", exact: true })
    .click();
  assert.ok(
    (await page.evaluate(() => navigator.clipboard.readText())).includes(
      "Direct shell",
    ),
  );
  await dock.getByRole("button", { name: "Ctrl+C", exact: true }).click();
  await page.waitForTimeout(400);
  assert.ok(
    writes.some(
      (write) =>
        write.path === "/api/terminals/input" && write.body.text === "\u0003",
    ),
  );
  assert.ok(
    writes.some(
      (write) =>
        write.path === "/api/terminals/resize" &&
        write.body.cols > 0 &&
        write.body.rows > 0,
    ),
  );
  await dock.getByRole("button", { name: "Terminal 251", exact: true }).click();
  await dock.getByLabel("Terminal name", { exact: true }).fill("Release logs");
  await dock.getByRole("button", { name: "Save", exact: true }).click();
  await dock
    .getByRole("button", { name: "Release logs", exact: true })
    .waitFor();
  const before = await dock.boundingBox();
  await dock.getByRole("separator").focus();
  await page.keyboard.press("ArrowUp");
  const after = await dock.boundingBox();
  assert.ok(after.height > before.height);
  await dock
    .getByRole("button", { name: "Hide terminals", exact: true })
    .click();
  await dock
    .getByRole("button", { name: "Show terminals", exact: true })
    .click();
  await page.waitForFunction(() =>
    document
      .querySelector(".xterm-accessibility-tree")
      ?.textContent.includes("terminal-ok"),
  );
  assert.equal(
    writes.filter((write) => write.path === "/api/terminals/create").length,
    1,
    "Hide and reconnect must not create a shell",
  );
  failOutput = true;
  await dock
    .getByText("Terminal connection interrupted", { exact: false })
    .waitFor();
  await dock.getByRole("button", { name: "Reconnect", exact: true }).click();
  await page.waitForFunction(() =>
    document
      .querySelector(".xterm-accessibility-tree")
      ?.textContent.includes("terminal-ok"),
  );
  uncertainInput = true;
  await dock.locator(".xterm-helper-textarea").focus();
  await page.keyboard.type("x");
  await dock
    .getByText(
      "Input delivery uncertain Input paused. Reconnect before you continue.",
    )
    .waitFor();
  const pausedCount = writes.filter(
    (write) => write.path === "/api/terminals/input",
  ).length;
  await page.keyboard.type("MUST_NOT_SEND");
  await page.waitForTimeout(200);
  assert.equal(
    writes.filter((write) => write.path === "/api/terminals/input").length,
    pausedCount,
  );
  await dock.getByRole("button", { name: "Reconnect", exact: true }).click();
  truncateOutput = true;
  await dock
    .getByText(
      "Earlier output exceeded the retained buffer. The latest output is shown.",
    )
    .waitFor();
  await page.screenshot({ path: join(root, "terminal-desktop.png") });
  await dock.getByLabel("Find terminal", { exact: true }).fill("npm run dev");
  await dock.getByText("No matching sessions", { exact: true }).waitFor();
  assert.equal(await dock.locator(".terminal-session").count(), 0);
  await dock
    .getByLabel("Find terminal", { exact: true })
    .fill("await deployment");
  await dock.getByText("No matching sessions", { exact: true }).waitFor();
  assert.equal(await dock.locator(".terminal-session").count(), 0);
  assert.equal(
    await dock
      .getByRole("button", { name: "Agent tools", exact: true })
      .count(),
    0,
  );
  assert.equal(
    writes.filter((write) =>
      ["/api/native-command", "/api/monitor/input"].includes(write.path),
    ).length,
    0,
  );
  await dock.getByLabel("Find terminal", { exact: true }).fill("Release logs");
  await dock
    .getByRole("button", { name: /Release logs Your terminal/ })
    .click();
  await page.setViewportSize({ width: 390, height: 844 });
  await page.screenshot({ path: join(root, "terminal-mobile.png") });
  assert.ok(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
    "Mobile view must not overflow",
  );
  assert.equal(
    await dock.count(),
    0,
    "Phone view has no desktop terminal dock",
  );
  await page.setViewportSize({ width: 1440, height: 980 });
  await dock.locator(".xterm-helper-textarea").waitFor();
  await dock.getByRole("button", { name: "End session", exact: true }).click();
  await dock
    .getByRole("button", {
      name: "End session and stop processes",
      exact: true,
    })
    .click();
  await dock
    .getByText("Your terminals, always here", { exact: true })
    .waitFor();
  assert.equal(
    writes.filter((write) => write.path === "/api/terminals/close").length,
    1,
  );
  assert.deepEqual(errors, []);
  console.log(
    JSON.stringify({
      ok: true,
      sessions: 250,
      checks: [
        "virtual list",
        "search all sessions",
        "direct input",
        "input order",
        "copy selection",
        "saved output download, pagination, cancellation, invalid cursor",
        "uncertain input pauses",
        "resize",
        "interrupt",
        "rename",
        "reconnect",
        "truncation",
        "agent commands excluded",
        "agent monitors excluded",
        "close",
        "mobile",
      ],
      screenshots: root,
    }),
  );
} finally {
  await browser?.close();
  await new Promise((resolve) => server.close(resolve));
}

if (process.argv.includes("--fixture-only")) process.exit(0);

// The same dock also exercises real shell input and saved output through the HTTP server.
const { spawn } = await import("node:child_process");
const liveRoot = await mkdtemp(join(tmpdir(), "codex-terminal-live-"));
const fixture = spawn(
  "python3",
  ["-B", join(skill, "tests/simple-ui-fixture.py"), liveRoot],
  { stdio: ["pipe", "pipe", "pipe"] },
);
let fixtureLog = "",
  liveBrowser,
  liveOrigin,
  liveToken,
  liveShell;
fixture.stderr.on("data", (data) => (fixtureLog += data));
try {
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (data) => resolve(Number(String(data).trim())));
    fixture.once("exit", () => reject(new Error(fixtureLog)));
  });
  liveOrigin = `http://127.0.0.1:${port}`;
  const initial = await (await fetch(liveOrigin + "/api/state")).json();
  liveToken = initial.token;
  liveBrowser = await chromium.launch({
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    headless: true,
  });
  const page = await liveBrowser.newPage({
    viewport: { width: 1280, height: 960 },
  });
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.goto(liveOrigin);
  assert.equal(await page.locator("#import-chat, #other-sessions").count(), 0);
  const dock = page.getByRole("region", { name: "Terminals", exact: true });
  await dock.getByRole("button", { name: "New terminal", exact: true }).click();
  await dock.locator(".xterm-helper-textarea").waitFor();
  await dock.locator(".xterm-helper-textarea").focus();
  await page.keyboard.type("printf 'PTY_RESULT:%s\\n' \"$PWD\"");
  await page.keyboard.press("Enter");
  await page.waitForFunction(() =>
    document
      .querySelector(".xterm-accessibility-tree")
      ?.textContent.includes("PTY_RESULT:/"),
  );
  const sessions = await (await fetch(liveOrigin + "/api/terminals")).json();
  assert.equal(sessions.items.length, 1);
  liveShell = sessions.items[0].id;
  const output = await (
    await fetch(
      `${liveOrigin}/api/terminals/output?id=${encodeURIComponent(liveShell)}&offset=0`,
    )
  ).json();
  assert.ok(output.text.includes(`PTY_RESULT:${sessions.items[0].cwd}`));
  await dock
    .getByRole("button", { name: "Hide terminals", exact: true })
    .click();
  await page.reload();
  await dock
    .getByRole("button", { name: "Show terminals", exact: true })
    .click();
  await page.waitForFunction(() =>
    document
      .querySelector(".xterm-accessibility-tree")
      ?.textContent.includes("PTY_RESULT:/"),
  );
  await dock.locator(".xterm-helper-textarea").focus();
  await page.keyboard.type("exit");
  await page.keyboard.press("Enter");
  await dock
    .getByText("Session ended · Output retained", { exact: true })
    .waitFor();
  const after = await (await fetch(liveOrigin + "/api/state")).json();
  const ownerBefore = initial.threads.find(
    (item) => item.id === sessions.items[0].agent,
  );
  const ownerAfter = after.threads.find(
    (item) => item.id === sessions.items[0].agent,
  );
  assert.equal(
    ownerBefore.turnId,
    ownerAfter.turnId,
    "A user shell must not start an agent turn",
  );
  assert.equal(ownerBefore.status, ownerAfter.status);
  await page.screenshot({ path: join(liveRoot, "terminal-live-desktop.png") });
  await dock.getByRole("button", { name: "End session", exact: true }).click();
  // This shell already exited. Closing its saved session needs no stop confirmation.
  await dock
    .getByText("Your terminals, always here", { exact: true })
    .waitFor();
  assert.deepEqual(errors, []);
  console.log(
    JSON.stringify({
      ok: true,
      livePTY: true,
      checks: [
        "actual shell input/output",
        "correct cwd",
        "output after page reload",
        "exit status",
        "no agent wake",
      ],
      screenshots: liveRoot,
    }),
  );
} finally {
  if (liveOrigin && liveToken) {
    const remaining = await fetch(liveOrigin + "/api/terminals", {
      signal: AbortSignal.timeout(3000),
    })
      .then((response) => response.json())
      .catch(() => ({ items: [] }));
    for (const session of remaining.items || []) {
      await fetch(liveOrigin + "/api/terminals/close", {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "X-Canvas-Token": liveToken,
          Origin: liveOrigin,
        },
        body: JSON.stringify({ id: session.id }),
        signal: AbortSignal.timeout(3000),
      }).catch(() => {});
    }
  }
  await liveBrowser?.close();
  fixture.kill("SIGTERM");
}

// Hidden product-renderer measurement. Test keys never enter the OS key store.
import assert from "node:assert/strict";
import { _electron as electron } from "playwright-core";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { execFileSync } from "node:child_process";
import { fixture } from "../web/tests/servers/multi-server-fixture.mjs";
const folder = await mkdtemp(path.join(tmpdir(), "studio-ms-memory-"));
const servers = await Promise.all([
  fixture("Remote", true),
  fixture("Third", true),
  fixture("Fourth", true),
]);
let desktop;
function memory(pid) {
  const rows = execFileSync("ps", ["-axo", "pid=,ppid=,rss=,args="], {
    encoding: "utf8",
  })
    .trim()
    .split("\n")
    .map((row) => {
      const parts = row.trim().match(/^(\d+)\s+(\d+)\s+(\d+)\s+(.*)$/);
      return (
        parts && {
          pid: Number(parts[1]),
          parent: Number(parts[2]),
          bytes: Number(parts[3]) * 1024,
          args: parts[4],
        }
      );
    })
    .filter(Boolean);
  const descendants = new Set([pid]);
  let changed = true;
  while (changed) {
    changed = false;
    for (const row of rows)
      if (descendants.has(row.parent) && !descendants.has(row.pid)) {
        descendants.add(row.pid);
        changed = true;
      }
  }
  const helpers = rows.filter(
    (row) => row.pid !== pid && descendants.has(row.pid),
  );
  return {
    helperResidentBytes: helpers.reduce((sum, row) => sum + row.bytes, 0),
    rendererResidentBytes: helpers
      .filter((row) => row.args.includes("--type=renderer"))
      .reduce((sum, row) => sum + row.bytes, 0),
    helperProcesses: helpers.length,
    rendererProcesses: helpers.filter((row) =>
      row.args.includes("--type=renderer"),
    ).length,
  };
}
try {
  assert.ok(
    process.env.CODEX_UI_EXECUTABLE,
    "Set CODEX_UI_EXECUTABLE to a separate UI-only package",
  );
  desktop = await electron.launch({
    executablePath: process.env.CODEX_UI_EXECUTABLE,
    args: ["--hidden"],
    env: {
      ...process.env,
      CODEX_UI_PORT: "0",
      CODEX_DESKTOP_PROFILE: path.join(folder, "profile"),
      CODEX_AGENTS_STATE_DIR: path.join(folder, "state"),
    },
  });
  await desktop.evaluate(
    ({ safeStorage }, destinations) => {
      safeStorage.isEncryptionAvailable = () => true;
      safeStorage.encryptString = (value) => Buffer.from(value);
      safeStorage.decryptString = (bytes) => bytes.toString();
      const original = globalThis.fetch;
      globalThis.fetch = async (input, init) => {
        const request = new Request(input, init),
          url = new URL(request.url);
        if (!destinations[url.origin]) return original(request);
        const body = await request.clone().arrayBuffer();
        return original(destinations[url.origin] + url.pathname + url.search, {
          method: request.method,
          headers: request.headers,
          ...(body.byteLength ? { body } : {}),
          signal: request.signal,
          redirect: "error",
        });
      };
    },
    Object.fromEntries(
      servers.map((server) => [server.invitation.origin, server.origin]),
    ),
  );
  const page = await desktop.firstWindow();
  page.setDefaultTimeout(15000);
  assert.equal(
    await desktop.evaluate(({ BrowserWindow }) =>
      BrowserWindow.getAllWindows()[0].isVisible(),
    ),
    false,
  );
  const pair = async (server) => {
    await page.locator("body").click({ position: { x: 1, y: 1 }, force: true });
    await page.evaluate(
      async ({ invitation }) => {
        const server = await window.codexDesktop.serverCredentialAction({
          action: "pair",
          origin: invitation.origin,
          invitation,
          requestId: crypto.randomUUID(),
        });
        const rows = JSON.parse(
          localStorage.getItem("studio-paired-servers-v1") || "[]",
        );
        localStorage.setItem(
          "studio-paired-servers-v1",
          JSON.stringify([...rows, server]),
        );
        localStorage.setItem("studio-selected-server", server.id);
        dispatchEvent(new Event("studio-server-registry"));
        document.querySelector('button[aria-label="Close"]')?.click();
      },
      { invitation: server.invitation },
    );
    await page
      .locator(`[data-server="${server.invitation.serverId}"] .server-chat`)
      .waitFor();
  };
  const measurements = [];
  await pair(servers[0]);
  await pair(servers[1]);
  await page.clock.install();
  for (const count of [2, 3]) {
    if (count === 3) await pair(servers[2]);
    await page.reload();
    for (const server of servers.slice(0, count))
      await page
        .frameLocator(`iframe[title="Studio on ${server.invitation.label}"]`)
        .locator("#message")
        .waitFor({ state: "attached" });
    await page.waitForFunction(
      (count) => document.querySelectorAll(".server-chat").length === count,
      count,
    );
    const mounted = {
      servers: count,
      stage: "mounted",
      frames: await page.locator("iframe").count(),
      ...memory(desktop.process().pid),
    };
    assert.equal(
      await desktop.evaluate(
        ({ BrowserWindow }) =>
          Object.keys(
            BrowserWindow.getAllWindows()[0].webContents.session.serviceWorkers.getAllRunning(),
          ).length,
      ),
      0,
    );
    await page.clock.fastForward(5 * 60 * 1000 + 30000);
    await page.waitForFunction(
      () => document.querySelectorAll("iframe").length === 1,
    );
    console.log(
      JSON.stringify({ waitingForQuietSeconds: 60, servers: count, mounted }),
    );
    await new Promise((resolve) => setTimeout(resolve, 60000));
    const idle = {
      servers: count,
      stage: "idle-unloaded",
      quietSeconds: 60,
      frames: await page.locator("iframe").count(),
      ...memory(desktop.process().pid),
    };
    measurements.push(mounted, idle);
    assert.equal(idle.frames, 1);
    assert.ok(
      idle.rendererProcesses < mounted.rendererProcesses,
      "Idle removal must release renderer processes",
    );
    assert.ok(
      idle.helperResidentBytes < mounted.helperResidentBytes,
      "Idle removal must reduce helper RSS",
    );
  }
  const result = {
    product: "packaged-ui-only",
    hidden: true,
    serviceWorkers: 0,
    keyStorage: "disposable fixture",
    measurements,
  };
  console.log(JSON.stringify(result));
  if (process.env.CODEX_MS_MEMORY_REPORT)
    await writeFile(
      process.env.CODEX_MS_MEMORY_REPORT,
      JSON.stringify(result, null, 2),
    );
} finally {
  if (desktop) {
    const pid = desktop.process().pid;
    const watchdog = setTimeout(() => {
      try {
        process.kill(process.platform === "win32" ? pid : -pid, "SIGKILL");
      } catch {}
    }, 10000);
    await desktop.close().catch(() => {});
    clearTimeout(watchdog);
  }
  await Promise.all(servers.map((server) => server.close()));
  await rm(folder, { recursive: true, force: true });
}

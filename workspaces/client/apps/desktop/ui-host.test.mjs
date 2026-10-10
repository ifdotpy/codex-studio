import assert from "node:assert/strict";
import { expect, test } from "vitest";
import { createRequire } from "node:module";
import { mkdtemp, mkdir, writeFile, rm, symlink } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
const require = createRequire(import.meta.url);
const { startUiHost } = require("./ui-host.cjs");
const { uiOnlyInstallation } = require("./install-mode.cjs");
test("UI-only host serves assets without backend resources and refuses API and outside files", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "studio-ui-only-"));
  await mkdir(path.join(root, "web/dist/assets"), { recursive: true });
  await writeFile(path.join(root, "web/dist/index.html"), "<h1>Studio UI</h1>");
  await writeFile(
    path.join(root, "web/dist/assets/app.js"),
    "export const ui = true;",
  );
  await writeFile(path.join(root, "private.txt"), "private");
  await symlink(
    path.join(root, "private.txt"),
    path.join(root, "web/dist/escape.txt"),
  );
  const host = await startUiHost({ resources: root, port: 0 });
  try {
    expect(host.origin).toMatch(/^http:\/\/127\.0\.0\.1:/);
    expect(
      await (await fetch(host.origin + "/?studio-ui-only=1")).text(),
    ).toContain("Studio UI");
    expect(
      (await fetch(host.origin + "/assets/app.js")).headers.get("Content-Type"),
    ).toContain("javascript");
    expect(
      (await fetch(host.origin + "/?studio-server=remote")).headers.get(
        "Content-Security-Policy",
      ),
    ).toContain("frame-ancestors 'self'");
    expect(
      (await fetch(host.origin + "/")).headers.get("Content-Security-Policy"),
    ).toContain("frame-ancestors 'none'");
    expect(
      (await fetch(host.origin + "/")).headers.get("Content-Security-Policy"),
    ).toContain("connect-src 'self' https://api.openai.com https://*.ts.net:*");
    expect((await fetch(host.origin + "/api/session")).status).toBe(404);
    expect((await fetch(host.origin + "/escape.txt")).status).toBe(404);
    expect((await fetch(host.origin + "/", { method: "POST" })).status).toBe(
      405,
    );
    await expect(
      startUiHost({ resources: root, port: Number(new URL(host.origin).port) }),
    ).rejects.toThrow("EADDRINUSE");
  } finally {
    await host.close();
    await rm(root, { recursive: true, force: true });
  }
});

test("UI host discovers the relocated development renderer", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "studio-ui-relocated-"));
  const development = path.join(
    root,
    "workspaces/client/apps/web/dist/index.html",
  );
  const packaged = path.join(root, "web/dist/index.html");
  await mkdir(path.dirname(development), { recursive: true });
  await mkdir(path.dirname(packaged), { recursive: true });
  await writeFile(development, "<h1>Development renderer</h1>");
  await writeFile(packaged, "<h1>Packaged renderer</h1>");
  const host = await startUiHost({ resources: root, port: 0 });
  try {
    const response = await fetch(host.origin);
    assert.equal(response.status, 200);
    assert.match(await response.text(), /Development renderer/);
  } finally {
    await host.close();
    await rm(root, { recursive: true, force: true });
  }
});
test("UI-only mode is explicit or belongs to the packaged artifact", async () => {
  expect(
    uiOnlyInstallation({ argv: ["--ui-only"], env: {}, packaged: false }),
  ).toBe(true);
  expect(uiOnlyInstallation({ argv: [], env: {}, packaged: false })).toBe(
    false,
  );
  const root = await mkdtemp(path.join(tmpdir(), "studio-ui-mode-"));
  try {
    await mkdir(path.join(root, "workspace"));
    await writeFile(
      path.join(root, "workspace/studio-install.json"),
      '{"mode":"ui-only"}',
    );
    expect(
      uiOnlyInstallation({
        argv: [],
        env: {},
        packaged: true,
        resourcesPath: root,
      }),
    ).toBe(true);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

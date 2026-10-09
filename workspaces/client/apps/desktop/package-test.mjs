import assert from "node:assert/strict";
import { _electron as electron } from "playwright-core";
import { access, mkdtemp, realpath } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { createServer } from "node:net";
import { execFileSync } from "node:child_process";
const root = path.dirname(fileURLToPath(import.meta.url));
const repositoryRoot = path.resolve(root, "../../../..");
const temp = await mkdtemp(path.join(tmpdir(), "codex-desktop-package-test-"));
const state = path.join(temp, "state");
const port = await new Promise((resolve) => {
  const server = createServer();
  server.listen(0, "127.0.0.1", () => {
    const port = server.address().port;
    server.close(() => resolve(port));
  });
});
const executable =
  process.env.CODEX_STUDIO_ELECTRON ||
  path.join(
    root,
    process.platform === "darwin"
      ? "dist/Codex Studio-darwin-arm64/Codex Studio.app/Contents/MacOS/Codex Studio"
      : `dist/Codex Studio-linux-${process.arch}/Codex Studio`,
  );
const uiOnly = process.env.CODEX_STUDIO_PACKAGE_UI_ONLY === "1";
const resources =
  process.platform === "darwin"
    ? path.resolve(path.dirname(executable), "../Resources/workspace")
    : path.resolve(path.dirname(executable), "resources/workspace");
const artifactRoot =
  process.platform === "darwin"
    ? path.resolve(path.dirname(executable), "../..")
    : path.dirname(executable);
const origin = `http://127.0.0.1:${port}`;
let desktop;
let pid;
try {
  desktop = await electron.launch({
    executablePath: executable,
    args: [
      "--hidden",
      ...(process.env.CODEX_STUDIO_OZONE_PLATFORM
        ? [`--ozone-platform=${process.env.CODEX_STUDIO_OZONE_PLATFORM}`]
        : []),
    ],
    env: {
      ...process.env,
      CODEX_AGENTS_STATE_DIR: state,
      CODEX_DESKTOP_PORT: String(port),
      CODEX_DESKTOP_PROFILE: path.join(temp, "profile"),
      ...(uiOnly ? { CODEX_UI_PORT: String(port) } : {}),
      ...(uiOnly ? { CODEX_DESKTOP_UI_ONLY: "1" } : {}),
    },
  });
  const page = await desktop.firstWindow();
  page.on("pageerror", (error) => console.error("packaged page error:", error));
  try {
    await page.waitForFunction(() => document.body.innerText.length > 0);
  } catch (error) {
    throw new Error(
      `${error.message}; page=${page.url()}; title=${await page.title()}; body=${(await page.locator("body").innerText()).slice(0, 500)}`,
      { cause: error },
    );
  }
  assert.equal(await desktop.evaluate(({ app }) => app.isPackaged), true);
  const artifactResourceRoot = await realpath(resources);
  const artifactRealRoot = await realpath(artifactRoot);
  const rendererRoot = await realpath(path.join(resources, "web/dist"));
  assert.ok(
    rendererRoot.startsWith(`${artifactResourceRoot}${path.sep}`),
    rendererRoot,
  );
  assert.ok(artifactResourceRoot.startsWith(`${artifactRealRoot}${path.sep}`));
  assert.ok(!artifactResourceRoot.startsWith(repositoryRoot));
  assert.equal(
    await desktop.evaluate(({ BrowserWindow }) =>
      BrowserWindow.getAllWindows()[0].isVisible(),
    ),
    false,
  );
  if (uiOnly) {
    assert.equal((await fetch(`${origin}/api/desktop`)).status, 404);
    await desktop.close();
    desktop = null;
    console.log(
      JSON.stringify({
        result: "PASS",
        artifact: executable,
        evidence: temp,
        resourceRoot: artifactResourceRoot,
        rendererRoot,
        stateDir: state,
        port,
        mode: "ui-only",
      }),
    );
  } else {
    assert.equal(
      await page.evaluate(() => typeof window.codexDesktop),
      "object",
    );
    const backendRoot = await realpath(path.join(resources, "scripts"));
    const backendEntry = await realpath(path.join(backendRoot, "codex-canvas"));
    const bridgeRoot = await realpath(path.join(backendRoot, "claude_bridge"));
    const bridgeEntry = await realpath(path.join(bridgeRoot, "bridge.mjs"));
    const bridgeModules = await realpath(path.join(bridgeRoot, "node_modules"));
    const bridgeSourceRoot = await realpath(
      path.join(resources, "workspaces/providers/apps/claude-bridge"),
    );
    assert.ok(backendEntry.startsWith(`${artifactResourceRoot}${path.sep}`));
    assert.ok(bridgeEntry.startsWith(`${bridgeRoot}${path.sep}`));
    assert.ok(bridgeModules.startsWith(`${bridgeRoot}${path.sep}`));
    assert.ok(
      bridgeSourceRoot.startsWith(`${artifactResourceRoot}${path.sep}`),
    );
    await Promise.all(
      [
        "requirements.txt",
        "prompts",
        "vm/guest",
        ".agents/skills",
        "package.json",
        "pnpm-workspace.yaml",
        "pnpm-lock.yaml",
        "patches/rxdb@17.5.0.patch",
        "workspaces/providers/apps/claude-bridge/package.json",
      ].map((relative) => access(path.join(resources, relative))),
    );
    await assert.rejects(
      access(path.join(backendRoot, ".studio-update.lock")),
      {
        code: "ENOENT",
      },
    );
    assert.equal(
      await page.evaluate(() =>
        ["requestMicrophone", "prepareTranscription", "transcribeAudio"].every(
          (method) => typeof window.codexDesktop[method] === "function",
        ),
      ),
      true,
    );
    const speech = JSON.parse(
      execFileSync(
        path.join(path.dirname(executable), "../Resources/studio-speech"),
        ["--check"],
        { encoding: "utf8" },
      ),
    );
    assert.equal(speech.helperReady, true);
    assert.equal(speech.onDeviceOnly, true);
    const roleSkills = JSON.parse(
      execFileSync(
        "python3",
        [
          "-B",
          "-c",
          `
import json, sys
sys.path.insert(0, sys.argv[1])
from codex_runtime import Runtime
print(json.dumps({name: Runtime.role_guidance({"isLead": lead}) for name, lead in [("orchestrator", True), ("subagent", False)]}))
`,
          path.join(path.dirname(executable), "../Resources/workspace/scripts"),
        ],
        { encoding: "utf8" },
      ),
    );
    assert.ok(
      roleSkills.orchestrator.includes(
        "[Studio role skill: codex-orchestrator]",
      ),
    );
    assert.ok(
      roleSkills.subagent.includes("[Studio role skill: codex-subagent]"),
    );
    const identity = await (await fetch(`${origin}/api/desktop`)).json();
    pid = identity.pid;
    assert.equal(identity.stateDir, await realpath(state));
    const command = execFileSync(
      "/bin/ps",
      ["-p", String(pid), "-o", "command="],
      { encoding: "utf8" },
    );
    assert.ok(
      command.includes(
        "Codex Studio.app/Contents/Resources/workspace/scripts/codex-canvas",
      ),
    );
    await desktop.close();
    desktop = null;
    assert.equal(
      (await (await fetch(`${origin}/api/desktop`)).json()).pid,
      pid,
    );
    console.log(
      JSON.stringify({
        result: "PASS",
        artifact: executable,
        evidence: temp,
        resourceRoot: artifactResourceRoot,
        rendererRoot,
        stateDir: state,
        port,
        backendPersistsAfterQuit: true,
      }),
    );
  }
} finally {
  if (desktop) await desktop.close();
  if (pid) process.kill(pid, "SIGTERM");
}

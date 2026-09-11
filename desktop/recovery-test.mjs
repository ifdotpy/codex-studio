import assert from "node:assert/strict";
import { test } from "node:test";
import {
  mkdtempSync,
  mkdirSync,
  writeFileSync,
  readFileSync,
  existsSync,
  rmSync,
  statSync,
  realpathSync,
} from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { createRequire } from "node:module";
import { execFileSync } from "node:child_process";
const { configureRecovery, recoveryPaths, recoveryPreference } = createRequire(
  import.meta.url,
)("./recovery.cjs");
function fixture() {
  const root = mkdtempSync(
    path.join(tmpdir(), "studio-recovery-registration-"),
  );
  const resources = path.join(root, "resources & source");
  mkdirSync(path.join(resources, "scripts"), { recursive: true });
  mkdirSync(path.join(resources, "web/dist"), { recursive: true });
  writeFileSync(path.join(resources, "scripts/codex-canvas"), "fixture");
  writeFileSync(path.join(resources, "web/dist/index.html"), "fixture");
  const supervisor = path.join(root, "recover_backend.py");
  writeFileSync(supervisor, "fixture");
  const env = {
    CODEX_AGENTS_STATE_DIR: path.join(root, "state"),
    PATH: process.env.PATH,
    CODEX_BIN: "/usr/bin/true",
    CODEX_HOME: path.join(root, "accounts"),
    CODEX_BOARD_STATE_DIR: path.join(root, "board"),
    OPENAI_API_KEY: "must-not-persist",
  };
  return { root, resources, supervisor, env, home: root, uid: 777 };
}

test("register and read back the exact-state launch agent without storing credentials", async () => {
  const fixtureData = fixture();
  const calls = [];
  let registered = false;
  try {
    const result = await configureRecovery({
      ...fixtureData,
      enabled: true,
      port: 4721,
      run: async (file, args) => {
        assert.equal(file, "/bin/launchctl");
        calls.push(args);
        if (args[0] === "print" && !registered) throw new Error("absent");
        if (args[0] === "bootstrap") registered = true;
      },
    });
    assert.equal(result.enabled, true);
    assert.deepEqual(
      calls.map((args) => args[0]),
      ["print", "bootstrap", "print"],
    );
    assert.equal(calls[1][1], "gui/777");
    const config = JSON.parse(readFileSync(result.config, "utf8"));
    assert.equal(config.environment.CODEX_HOME, fixtureData.env.CODEX_HOME);
    assert.equal(
      config.environment.CODEX_BOARD_STATE_DIR,
      fixtureData.env.CODEX_BOARD_STATE_DIR,
    );
    assert.equal(config.environment.OPENAI_API_KEY, undefined);
    assert.equal(statSync(result.config).mode & 0o777, 0o600);
    const plist = JSON.parse(
      execFileSync(
        "/usr/bin/plutil",
        ["-convert", "json", "-o", "-", result.plist],
        { encoding: "utf8" },
      ),
    );
    assert.equal(plist.KeepAlive, true);
    assert.equal(plist.AbandonProcessGroup, true);
    assert.equal(plist.ProgramArguments[2], fixtureData.supervisor);
    assert.equal(plist.ProgramArguments[4], result.config);
    assert.equal(plist.Label, result.label);
  } finally {
    rmSync(fixtureData.root, { recursive: true, force: true });
  }
});

test("updating registered recovery preserves its live service; disabling removes only supervision", async () => {
  const data = fixture();
  const calls = [];
  try {
    const run = async (_, args) => {
      calls.push(args[0]);
    };
    const result = await configureRecovery({ ...data, enabled: true, run });
    assert.deepEqual(calls, ["print", "print"]);
    calls.length = 0;
    await configureRecovery({ ...data, enabled: false, run });
    assert.deepEqual(calls, ["print", "bootout"]);
    assert.equal(recoveryPreference(data.env), false);
    assert.equal(existsSync(result.plist), false);
    assert.equal(JSON.parse(readFileSync(result.config)).enabled, false);
  } finally {
    rmSync(data.root, { recursive: true, force: true });
  }
});

test("launchd registration failure is an error and cannot report success", async () => {
  const data = fixture();
  try {
    await assert.rejects(
      configureRecovery({
        ...data,
        enabled: true,
        run: async () => {
          throw new Error("launchd rejected");
        },
      }),
      /launchd rejected/,
    );
    const files = recoveryPaths(data.env, data.home);
    assert.equal(existsSync(files.config), true);
  } finally {
    rmSync(data.root, { recursive: true, force: true });
  }
});

test("the hidden desktop replaces a crashed renderer and preserves the backend", async () => {
  const { execFile } = await import("node:child_process");
  const { promisify } = await import("node:util");
  const { createServer } = await import("node:net");
  const root = path.dirname(new URL(import.meta.url).pathname);
  const temp = mkdtempSync(path.join(tmpdir(), "studio-renderer-recovery-"));
  const port = await new Promise((resolve) => {
    const server = createServer();
    server.listen(0, "127.0.0.1", () => {
      const port = server.address().port;
      server.close(() => resolve(port));
    });
  });
  let pid;
  try {
    const executable = createRequire(import.meta.url)("electron");
    const { stdout } = await promisify(execFile)(
      executable,
      [path.join(root, "recovery-renderer-test.cjs"), "--hidden"],
      {
        timeout: 30000,
        env: {
          ...process.env,
          CODEX_AGENTS_STATE_DIR: path.join(temp, "state"),
          CODEX_BOARD_STATE_DIR: path.join(temp, "board"),
          CODEX_DESKTOP_PROFILE: path.join(temp, "profile"),
          CODEX_DESKTOP_PORT: String(port),
        },
      },
    );
    const result = stdout
      .split("\n")
      .filter(Boolean)
      .map((line) => JSON.parse(line))
      .find((record) => record.rendererRecovered);
    assert.ok(result);
    pid = result.backendPid;
  } finally {
    if (!pid) {
      try {
        const state = JSON.parse(
          await (await fetch(`http://127.0.0.1:${port}/api/desktop`)).text(),
        );
        if (state.stateDir === realpathSync(path.join(temp, "state")))
          pid = state.pid;
      } catch {}
    }
    if (pid) process.kill(pid, "SIGTERM");
    // Keep this isolated crash fixture and its logs for inspection.
  }
});

test("a graceful desktop exit always withdraws open intent", async () => {
  const { EventEmitter } = await import("node:events");
  const { trackDesktopRecovery } = createRequire(import.meta.url)(
    "./recovery.cjs",
  );
  for (const shutdown of [false, true]) {
    const data = fixture();
    try {
      const app = new EventEmitter();
      app.getPath = () => path.join(data.root, "profile");
      const powerMonitor = new EventEmitter();
      const { filename } = trackDesktopRecovery({
        app,
        powerMonitor,
        env: data.env,
      });
      assert.equal(JSON.parse(readFileSync(filename)).desiredOpen, true);
      if (shutdown) powerMonitor.emit("shutdown");
      app.emit("before-quit");
      app.emit("window-all-closed");
      assert.equal(JSON.parse(readFileSync(filename)).desiredOpen, false);
    } finally {
      rmSync(data.root, { recursive: true, force: true });
    }
  }
});

test("backend restart environment overrides the attaching desktop and preserves absent keys", async () => {
  const data = fixture();
  try {
    const result = await configureRecovery({
      ...data,
      enabled: true,
      restartEnvironment: {
        CODEX_BIN: "/usr/bin/true",
        CODEX_BOARD_STATE_DIR: "/authoritative-board",
      },
      run: async () => {},
    });
    const config = JSON.parse(readFileSync(result.config));
    assert.equal(
      config.environment.CODEX_BOARD_STATE_DIR,
      "/authoritative-board",
    );
    assert.equal(config.environment.CODEX_HOME, undefined);
    assert.ok(config.unsetEnvironment.includes("CODEX_HOME"));
  } finally {
    rmSync(data.root, { recursive: true, force: true });
  }
});

test("explicit Quit and window close withdraw intent after a cancelled shutdown", async () => {
  const { EventEmitter } = await import("node:events");
  const { trackDesktopRecovery } = createRequire(import.meta.url)(
    "./recovery.cjs",
  );
  for (const action of ["closeExplicitly", "windowClosing"]) {
    const data = fixture();
    try {
      const app = new EventEmitter();
      app.getPath = () => path.join(data.root, "profile");
      const powerMonitor = new EventEmitter();
      const tracker = trackDesktopRecovery({
        app,
        powerMonitor,
        env: data.env,
      });
      powerMonitor.emit("shutdown");
      tracker[action]();
      app.emit("before-quit");
      app.emit("window-all-closed");
      assert.equal(
        JSON.parse(readFileSync(tracker.filename)).desiredOpen,
        false,
      );
    } finally {
      rmSync(data.root, { recursive: true, force: true });
    }
  }
});

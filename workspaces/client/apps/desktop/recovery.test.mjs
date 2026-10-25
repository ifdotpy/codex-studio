import assert from "node:assert/strict";
import { test } from "vitest";
import {
  mkdtempSync,
  mkdirSync,
  writeFileSync,
  readFileSync,
  existsSync,
  rmSync,
  statSync,
  realpathSync,
  chmodSync,
} from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { createRequire } from "node:module";
import { execFileSync } from "node:child_process";
const {
  configureRecovery,
  recoveryPaths,
  recoveryPreference,
  supervisorPreference,
  isInstalledApplication,
  recoveryStatusLabel,
} = createRequire(import.meta.url)("./recovery.cjs");
function fixture() {
  const root = mkdtempSync(
    path.join(tmpdir(), "studio-recovery-registration-"),
  );
  const resources = path.join(root, "resources & source");
  mkdirSync(path.join(resources, "bin"), { recursive: true });
  mkdirSync(path.join(resources, "web/dist"), { recursive: true });
  for (const name of [
    "studio-runtime-server",
    "codex-process-supervisor",
    "codex",
  ]) {
    const binary = path.join(resources, "bin", name);
    writeFileSync(binary, "#!/bin/sh\nexit 0\n");
    chmodSync(binary, 0o755);
  }
  writeFileSync(path.join(resources, "web/dist/index.html"), "fixture");
  const supervisor = path.join(root, "recover_backend.py");
  writeFileSync(supervisor, "fixture");
  const env = {
    CODEX_AGENTS_STATE_DIR: path.join(root, "state"),
    PATH: process.env.PATH,
    CODEX_BIN: path.join(resources, "bin/codex"),
    CODEX_AGENTS_PYTHON: "/nonexistent",
    CODEX_HOME: path.join(root, "accounts"),
    CODEX_CANVAS_CWD: path.join(root, "workspace"),
    CODEX_BOARD_STATE_DIR: path.join(root, "legacy-board"),
    OPENAI_API_KEY: "must-not-persist",
  };
  return {
    root,
    resources,
    supervisor,
    env,
    home: root,
    uid: 777,
    applicationPath:
      "/Applications/Codex Studio.app/Contents/Resources/app.asar",
    // The fixture supervisor is not a real script: accept every interpreter.
    checkInterpreter: async () => {},
  };
}

// Two python3 stubs on PATH: `old` stands for /usr/bin/python3 (3.9), `new` for a
// Python that runs recover_backend.py.
function interpreters(data) {
  const found = {};
  for (const name of ["old", "new"]) {
    const dir = path.join(data.root, `${name}-python`);
    mkdirSync(dir, { recursive: true });
    found[name] = path.join(dir, "python3");
    writeFileSync(found[name], "#!/bin/sh\nexit 0\n");
    chmodSync(found[name], 0o755);
  }
  return found;
}
function plistInterpreter(file) {
  return readFileSync(file, "utf8").match(
    /<key>ProgramArguments<\/key><array><string>([^<]+)<\/string>/,
  )[1];
}

test("only the installed Studio bundle can configure background recovery", async () => {
  const data = fixture();
  const state = path.join(data.root, "state");
  const calls = [];
  try {
    assert.equal(isInstalledApplication(data.applicationPath), true);
    assert.equal(
      isInstalledApplication(
        "/Applications/Codex Studio.app/Contents/MacOS/studio-native-client",
      ),
      true,
    );
    assert.equal(
      isInstalledApplication(
        "/tmp/Codex Studio.app/Contents/MacOS/studio-native-client",
      ),
      false,
    );
    assert.equal(
      isInstalledApplication(path.join(data.root, "Codex Studio.app/app.asar")),
      false,
    );
    assert.equal(
      isInstalledApplication(
        "/Users/test/Library/Caches/chrompile/CodexStudio-Electron-Isolated.app/Contents/Resources/app.asar",
      ),
      false,
    );
    for (const enabled of [true, false]) {
      const attempt = {
        ...data,
        applicationPath: path.join(
          data.root,
          "Library/Caches/test-copy/Contents/Resources/app.asar",
        ),
        env: { ...data.env, CODEX_AGENTS_STATE_DIR: state },
        enabled,
        run: async (...args) => calls.push(args),
      };
      await assert.rejects(
        configureRecovery(attempt),
        /only by \/Applications\/Codex Studio\.app/,
      );
    }
    assert.deepEqual(calls, []);
    assert.equal(
      existsSync(path.join(state, "background-recovery.json")),
      false,
    );
    assert.equal(
      existsSync(path.join(data.home, "Library/LaunchAgents")),
      false,
    );
    assert.equal(
      recoveryStatusLabel(false, false),
      "Background recovery: Unavailable",
    );
    assert.equal(recoveryStatusLabel(true), "Background recovery: On");
    assert.equal(recoveryStatusLabel(false), "Background recovery: Off");
  } finally {
    rmSync(data.root, { recursive: true, force: true });
  }
});

test.skipIf(process.platform !== "darwin")(
  "register and read back the exact-state launch agent without storing credentials",
  async () => {
    const fixtureData = fixture();
    const calls = [];
    const registered = new Set();
    const paths = recoveryPaths(fixtureData.env, fixtureData.home);
    try {
      const result = await configureRecovery({
        ...fixtureData,
        enabled: true,
        port: 4721,
        run: async (file, args) => {
          assert.equal(file, "/bin/launchctl");
          calls.push(args);
          if (args[0] === "print" && !registered.has(args[1]))
            throw new Error("absent");
          if (args[0] === "bootstrap")
            registered.add(
              args[2] === paths.supervisorPlist
                ? `gui/777/${paths.supervisorLabel}`
                : `gui/777/${paths.label}`,
            );
        },
      });
      assert.equal(result.enabled, true);
      assert.deepEqual(
        calls.map((args) => args[0]),
        ["print", "bootstrap", "print"],
      );
      assert.equal(calls[1][1], "gui/777");
      const config = JSON.parse(readFileSync(result.config, "utf8"));
      assert.equal(config.supervisorEnabled, false);
      assert.equal(config.environment.CODEX_HOME, fixtureData.env.CODEX_HOME);
      assert.equal(
        config.environment.CODEX_CANVAS_CWD,
        fixtureData.env.CODEX_CANVAS_CWD,
      );
      assert.equal(config.environment.OPENAI_API_KEY, undefined);
      assert.equal(config.environment.CODEX_BOARD_STATE_DIR, undefined);
      assert.equal(statSync(result.config).mode & 0o777, 0o600);
      const plist = JSON.parse(
        execFileSync(
          "/usr/bin/plutil",
          ["-convert", "json", "-o", "-", result.plist],
          { encoding: "utf8" },
        ),
      );
      assert.equal(plist.KeepAlive, true);
      assert.equal(plist.ProcessType, "Interactive");
      assert.equal(plist.AbandonProcessGroup, true);
      assert.equal(plist.ProgramArguments[2], fixtureData.supervisor);
      assert.equal(plist.ProgramArguments[4], result.config);
      assert.equal(plist.Label, result.label);
    } finally {
      rmSync(fixtureData.root, { recursive: true, force: true });
    }
  },
);

test.skipIf(process.platform !== "darwin")(
  "supervisor mode is opt-in in the saved launchd configuration",
  async () => {
    const data = fixture();
    let registered = false;
    try {
      const result = await configureRecovery({
        ...data,
        enabled: true,
        restartEnvironment: { CODEX_AGENTS_SUPERVISOR_MODE: "1" },
        run: async (_file, args) => {
          if (args[0] === "print" && !registered) throw new Error("absent");
          if (args[0] === "bootstrap") registered = true;
        },
      });
      const config = JSON.parse(readFileSync(result.config, "utf8"));
      assert.equal(config.supervisorEnabled, true);
      assert.equal(config.environment.CODEX_AGENTS_SUPERVISOR_MODE, undefined);
      const supervisorPlist = JSON.parse(
        execFileSync(
          "/usr/bin/plutil",
          ["-convert", "json", "-o", "-", result.supervisorPlist],
          { encoding: "utf8" },
        ),
      );
      assert.equal(supervisorPlist.KeepAlive, true);
      assert.equal(supervisorPlist.ProcessType, "Interactive");
      assert.equal(supervisorPlist.AbandonProcessGroup, true);
      assert.equal(supervisorPlist.Label, result.supervisorLabel);
      assert.deepEqual(supervisorPlist.ProgramArguments.slice(-3), [
        "--state",
        result.state,
        "--wait-for-lease",
      ]);
      await configureRecovery({
        ...data,
        enabled: true,
        run: async () => {},
      });
      const relaunched = JSON.parse(readFileSync(result.config, "utf8"));
      assert.equal(relaunched.supervisorEnabled, true);
    } finally {
      rmSync(data.root, { recursive: true, force: true });
    }
  },
);

test("a resource policy update preserves the registered recovery process", async () => {
  const data = fixture();
  const calls = [];
  try {
    const initial = await configureRecovery({
      ...data,
      enabled: true,
      run: async () => {},
    });
    const nextPlist = readFileSync(initial.plist, "utf8");
    writeFileSync(
      initial.plist,
      nextPlist.replace(
        "<key>ProcessType</key><string>Interactive</string>\n",
        "",
      ),
    );
    await configureRecovery({
      ...data,
      enabled: true,
      run: async (_file, args) => {
        calls.push(args);
        return { stdout: "pid = 4242\n" };
      },
    });
    assert.deepEqual(
      calls.map((args) => args[0]),
      ["print", "print"],
    );
    assert.equal(readFileSync(initial.plist, "utf8"), nextPlist);
  } finally {
    rmSync(data.root, { recursive: true, force: true });
  }
});

test.each([false, true])(
  "a lost bootstrap response preserves registration or restores the previous service (%s)",
  async (applied) => {
    const data = fixture();
    const calls = [];
    let registered = true;
    let bootstraps = 0;
    try {
      const initial = await configureRecovery({
        ...data,
        enabled: true,
        run: async () => {},
      });
      const previousPlist = readFileSync(initial.plist, "utf8");
      const replacement = path.join(data.root, "recover_backend-v2.py");
      writeFileSync(replacement, "fixture v2");
      const update = configureRecovery({
        ...data,
        supervisor: replacement,
        enabled: true,
        run: async (_file, args) => {
          calls.push(args[0]);
          if (args[0] === "print" && !registered)
            throw Object.assign(new Error("service is absent"), { code: 113 });
          if (args[0] === "bootout") registered = false;
          if (args[0] === "bootstrap") {
            bootstraps += 1;
            registered = applied || bootstraps > 1;
            if (bootstraps === 1)
              throw Object.assign(new Error("Bootstrap failed: 5"), {
                code: 5,
              });
          }
          return { stdout: "pid = 4242\n" };
        },
      });
      if (applied) {
        await update;
        assert.equal(bootstraps, 1);
        assert.equal(
          readFileSync(initial.plist, "utf8").includes(replacement),
          true,
        );
      } else {
        await assert.rejects(update, /previous recovery service was restored/);
        assert.equal(bootstraps, 2);
        assert.equal(readFileSync(initial.plist, "utf8"), previousPlist);
      }
      assert.equal(registered, true);
      assert.equal(calls.filter((action) => action === "bootout").length, 1);
    } finally {
      rmSync(data.root, { recursive: true, force: true });
    }
  },
);

test.each([false, true])(
  "recovery does not retry an unknown registration and reports a failed restore (%s)",
  async (absent) => {
    const data = fixture();
    let bootedOut = false;
    let bootstraps = 0;
    try {
      const initial = await configureRecovery({
        ...data,
        enabled: true,
        run: async () => {},
      });
      const previousPlist = readFileSync(initial.plist, "utf8");
      const replacement = path.join(data.root, "recover_backend-v2.py");
      writeFileSync(replacement, "fixture v2");
      await assert.rejects(
        configureRecovery({
          ...data,
          supervisor: replacement,
          enabled: true,
          run: async (_file, args) => {
            if (args[0] === "print" && bootedOut)
              throw Object.assign(new Error("registration is unknown"), {
                code: absent ? 113 : 1,
              });
            if (args[0] === "bootout") bootedOut = true;
            if (args[0] === "bootstrap") {
              bootstraps += 1;
              throw Object.assign(new Error("Bootstrap failed: 5"), {
                code: 5,
              });
            }
            return { stdout: "pid = 4242\n" };
          },
        }),
        absent ? /Cannot update or restore/ : /Bootstrap failed: 5/,
      );
      assert.equal(bootstraps, absent ? 2 : 1);
      if (absent)
        assert.equal(readFileSync(initial.plist, "utf8"), previousPlist);
      else
        assert.equal(
          readFileSync(initial.plist, "utf8").includes(replacement),
          true,
        );
    } finally {
      rmSync(data.root, { recursive: true, force: true });
    }
  },
);

test("recovery rewrite refuses to stop its supervisor while handles are live", async () => {
  const data = fixture();
  const paths = recoveryPaths(data.env, data.home);
  const registered = new Set();
  const calls = [];
  try {
    const run = async (file, args) => {
      calls.push([file, args]);
      if (file === "/bin/launchctl") {
        const service = args[1];
        if (args[0] === "print" && !registered.has(service))
          throw new Error("service is not loaded");
        if (args[0] === "bootstrap")
          registered.add(
            `${args[1]}/${args[2].includes("supervisor.") ? paths.supervisorLabel : paths.label}`,
          );
        if (args[0] === "print" && service === `gui/777/${paths.label}`)
          return { stdout: "pid = 4242\n" };
        return { stdout: "" };
      }
      if (file === "/bin/ps") return { stdout: "4242\n" };
      return {
        stdout: JSON.stringify({ handles: [{ id: "live", pid: 8888 }] }),
      };
    };
    const initial = await configureRecovery({
      ...data,
      enabled: true,
      restartEnvironment: { CODEX_AGENTS_SUPERVISOR_MODE: "1" },
      run,
    });
    writeFileSync(
      path.join(initial.state, "supervisor.lock"),
      JSON.stringify({ pid: 9001, startTime: "fixture-start" }),
    );
    const originalPlist = readFileSync(initial.plist, "utf8");
    writeFileSync(path.join(data.root, "recover_backend-v2.py"), "fixture v2");
    await assert.rejects(
      configureRecovery({
        ...data,
        supervisor: path.join(data.root, "recover_backend-v2.py"),
        enabled: true,
        run,
      }),
      /supervisor pid \d+ owns 1 live handle/,
    );
    assert.equal(readFileSync(initial.plist, "utf8"), originalPlist);
    assert.equal(
      calls.some(
        ([file, args]) =>
          file === "/bin/launchctl" &&
          args[0] === "bootout" &&
          args[1] === `gui/777/${paths.label}`,
      ),
      false,
    );
  } finally {
    rmSync(data.root, { recursive: true, force: true });
  }
});

test("a Finder launch cannot turn off the persisted supervisor setting", async () => {
  const data = fixture();
  try {
    const files = recoveryPaths(data.env, data.home);
    mkdirSync(path.dirname(files.config), { recursive: true });
    writeFileSync(
      files.config,
      JSON.stringify({ version: 1, enabled: true, supervisorEnabled: true }),
    );
    await configureRecovery({ ...data, enabled: true, run: async () => {} });
    assert.equal(
      JSON.parse(readFileSync(files.config)).supervisorEnabled,
      true,
    );
    assert.equal(supervisorPreference(data.env), true);
    assert.equal(
      supervisorPreference({
        ...data.env,
        CODEX_AGENTS_SUPERVISOR_MODE: "0",
      }),
      false,
    );
  } finally {
    rmSync(data.root, { recursive: true, force: true });
  }
});

test("a desktop launch keeps the service-owned Rust selection and its supervisor job", async () => {
  const data = fixture();
  try {
    const files = recoveryPaths(data.env, data.home);
    mkdirSync(path.dirname(files.config), { recursive: true });
    const selection = {
      backend: "rust",
      runtimeServer: path.join(data.resources, "bin/studio-runtime-server"),
      processSupervisor: path.join(
        data.resources,
        "bin/codex-process-supervisor",
      ),
    };
    writeFileSync(
      files.config,
      JSON.stringify({
        version: 1,
        enabled: true,
        supervisorEnabled: true,
        ...selection,
      }),
    );
    const calls = [];
    await configureRecovery({
      ...data,
      enabled: true,
      run: async (_file, args) => {
        calls.push(args);
      },
    });
    const saved = JSON.parse(readFileSync(files.config));
    assert.deepEqual(
      {
        backend: saved.backend,
        runtimeServer: saved.runtimeServer,
        processSupervisor: saved.processSupervisor,
      },
      selection,
    );
    assert.equal(saved.supervisorEnabled, true);
    assert.equal(existsSync(files.supervisorPlist), false);
    assert.equal(
      calls.some((args) => args.join(" ").includes(files.supervisorLabel)),
      false,
    );
  } finally {
    rmSync(data.root, { recursive: true, force: true });
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

test.skipIf(!process.env.CODEX_DESKTOP_E2E)(
  "the hidden desktop survives crash loops and supports native reload",
  async () => {
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
          timeout: 125000,
          env: {
            ...process.env,
            CODEX_AGENTS_STATE_DIR: path.join(temp, "state"),
            CODEX_AGENTS_SUPERVISOR_MODE: "0",
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
  },
);

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

test("an attaching desktop preserves the saved restart environment and explicit absence", async () => {
  const data = fixture();
  try {
    const initial = await configureRecovery({
      ...data,
      enabled: true,
      restartEnvironment: {
        CODEX_BIN: "/usr/bin/true",
        CODEX_CANVAS_CWD: "/saved-workspace",
      },
      run: async () => {},
    });
    const before = JSON.parse(readFileSync(initial.config));
    await configureRecovery({ ...data, enabled: true, run: async () => {} });
    const after = JSON.parse(readFileSync(initial.config));
    assert.deepEqual(after.environment, before.environment);
    assert.deepEqual(after.unsetEnvironment, before.unsetEnvironment);
    assert.equal(after.environment.CODEX_HOME, undefined);
    assert.ok(after.unsetEnvironment.includes("CODEX_HOME"));
    assert.equal(after.environment.CODEX_CANVAS_CWD, "/saved-workspace");
  } finally {
    rmSync(data.root, { recursive: true, force: true });
  }
});

test("recovery rejects an invalid saved launch environment before changing it", async () => {
  const data = fixture();
  const files = recoveryPaths(data.env, data.home);
  const valid = {
    version: 1,
    stateDir: files.state,
    environment: { CODEX_BIN: "/usr/bin/true", PATH: data.env.PATH },
    unsetEnvironment: ["CODEX_HOME"],
  };
  try {
    for (const invalid of [
      { ...valid, version: 2 },
      { ...valid, stateDir: path.join(data.root, "another-state") },
      { ...valid, environment: [] },
      { ...valid, environment: { CODEX_HOME: 1 } },
      { ...valid, environment: { OPENAI_API_KEY: "unexpected" } },
      { ...valid, unsetEnvironment: "CODEX_HOME" },
      { ...valid, unsetEnvironment: ["PATH"] },
      { ...valid, unsetEnvironment: ["CODEX_BIN"] },
    ]) {
      const before = JSON.stringify(invalid);
      writeFileSync(files.config, before);
      await assert.rejects(
        configureRecovery({
          ...data,
          enabled: true,
          run: async () =>
            assert.fail("Invalid state must not reach launchctl"),
        }),
        /saved restart environment/i,
      );
      assert.equal(readFileSync(files.config, "utf8"), before);
    }
  } finally {
    rmSync(data.root, { recursive: true, force: true });
  }
});

// The shapes of two live background-recovery.json files that the deleted
// Python backend wrote (one Mac still on Python, one already switched to Rust;
// values replaced by fixture paths). Main refused both for CODEX_AGENTS_PYTHON.
function pythonEraRecords(state, resources) {
  const codex = path.join(resources, "bin/codex");
  return [
    {
      version: 1,
      enabled: true,
      supervisorEnabled: true,
      stateDir: state,
      resources,
      python: "/opt/homebrew/bin/python3",
      codex,
      port: 4620,
      environment: {
        SHELL: "/bin/zsh",
        PATH: path.dirname(codex),
      },
      unsetEnvironment: [
        "CODEX_HOME",
        "CODEX_CANVAS_CWD",
        "CODEX_CANVAS_CONCURRENCY",
        "CODEX_BIN",
        "CODEX_AGENTS_PYTHON",
        "LANG",
        "LC_ALL",
        "CODEX_AGENTS_SUPERVISOR_MODE",
        "CODEX_AGENTS_SUPERVISOR_FALLBACK",
      ],
    },
    {
      version: 1,
      enabled: true,
      supervisorEnabled: true,
      stateDir: state,
      resources,
      python: "/opt/homebrew/bin/python3",
      codex,
      port: 4620,
      environment: {
        CODEX_BIN: codex,
        SHELL: "/bin/zsh",
        LANG: "en_US.UTF-8",
        LC_ALL: "en_US.UTF-8",
        PATH: path.dirname(codex),
      },
      unsetEnvironment: [
        "CODEX_HOME",
        "CODEX_CANVAS_CWD",
        "CODEX_CANVAS_CONCURRENCY",
        "CODEX_AGENTS_PYTHON",
        "CODEX_AGENTS_SUPERVISOR_MODE",
        "CODEX_AGENTS_SUPERVISOR_FALLBACK",
      ],
      backend: "rust",
      runtimeServer: path.join(resources, "bin/studio-runtime-server"),
      processSupervisor: path.join(resources, "bin/codex-process-supervisor"),
    },
  ];
}

test("a Python-era saved launch environment is accepted without its retired keys", async () => {
  const data = fixture();
  const files = recoveryPaths(data.env, data.home);
  try {
    for (const saved of pythonEraRecords(files.state, data.resources)) {
      writeFileSync(files.config, JSON.stringify(saved));
      await configureRecovery({ ...data, enabled: true, run: async () => {} });
      const after = JSON.parse(readFileSync(files.config, "utf8"));
      assert.equal(after.backend, "rust");
      assert.ok(!after.unsetEnvironment.includes("CODEX_AGENTS_PYTHON"));
      assert.equal(after.environment.CODEX_AGENTS_PYTHON, undefined);
      assert.equal(after.environment.SHELL, "/bin/zsh");
      assert.ok(after.unsetEnvironment.includes("CODEX_HOME"));
    }
    // A retired name with a saved value is accepted and not restored.
    const [first] = pythonEraRecords(files.state, data.resources);
    writeFileSync(
      files.config,
      JSON.stringify({
        ...first,
        environment: {
          ...first.environment,
          CODEX_AGENTS_PYTHON: "/old/python",
        },
        unsetEnvironment: first.unsetEnvironment.filter(
          (key) => key !== "CODEX_AGENTS_PYTHON",
        ),
      }),
    );
    await configureRecovery({ ...data, enabled: true, run: async () => {} });
    const after = JSON.parse(readFileSync(files.config, "utf8"));
    assert.equal(after.environment.CODEX_AGENTS_PYTHON, undefined);
    // An unknown new name is still refused.
    const before = JSON.stringify({
      ...first,
      unsetEnvironment: [...first.unsetEnvironment, "CODEX_UNKNOWN_SETTING"],
    });
    writeFileSync(files.config, before);
    await assert.rejects(
      configureRecovery({
        ...data,
        enabled: true,
        run: async () => assert.fail("Invalid state must not reach launchctl"),
      }),
      /saved restart environment/i,
    );
    assert.equal(readFileSync(files.config, "utf8"), before);
  } finally {
    rmSync(data.root, { recursive: true, force: true });
  }
});

test("backend restart environment overrides the attaching desktop and preserves absent keys", async () => {
  const data = fixture();
  try {
    await configureRecovery({ ...data, enabled: true, run: async () => {} });
    const result = await configureRecovery({
      ...data,
      enabled: true,
      restartEnvironment: {
        CODEX_BIN: "/usr/bin/true",
        CODEX_CANVAS_CWD: "/authoritative-workspace",
        CODEX_BOARD_STATE_DIR: "/legacy-board",
      },
      run: async () => {},
    });
    const config = JSON.parse(readFileSync(result.config));
    assert.equal(
      config.environment.CODEX_CANVAS_CWD,
      "/authoritative-workspace",
    );
    assert.equal(config.environment.CODEX_HOME, undefined);
    assert.equal(config.environment.CODEX_BOARD_STATE_DIR, undefined);
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

test("recovery refuses when no interpreter runs recover_backend.py and writes nothing", async () => {
  const data = fixture();
  const calls = [];
  const paths = recoveryPaths(data.env, data.home);
  try {
    await assert.rejects(
      configureRecovery({
        ...data,
        enabled: true,
        checkInterpreter: async () => {
          throw new Error("ImportError: cannot import name 'StrEnum'");
        },
        run: async (...args) => calls.push(args),
      }),
      /needs Python 3\.11 or newer to run recover_backend\.py/,
    );
    assert.deepEqual(calls, []);
    assert.equal(existsSync(paths.config), false);
    assert.equal(existsSync(paths.plist), false);
    // Turning recovery off never needs a supported interpreter.
    const off = await configureRecovery({
      ...data,
      enabled: false,
      checkInterpreter: async () => {
        throw new Error("ImportError: cannot import name 'StrEnum'");
      },
      run: async () => {
        throw new Error("absent");
      },
    });
    assert.equal(off.enabled, false);
  } finally {
    rmSync(data.root, { recursive: true, force: true });
  }
});

test("recovery saves the first interpreter that runs recover_backend.py", async () => {
  const data = fixture();
  const python = interpreters(data);
  const checked = [];
  try {
    const result = await configureRecovery({
      ...data,
      env: {
        ...data.env,
        PATH: `${path.dirname(python.old)}:${path.dirname(python.new)}`,
      },
      enabled: true,
      checkInterpreter: async (candidate, script) => {
        checked.push([candidate, script]);
        if (candidate === python.old)
          throw new Error("ImportError: cannot import name 'StrEnum'");
      },
      run: async () => {},
    });
    assert.deepEqual(checked.slice(0, 2), [
      [python.old, data.supervisor],
      [python.new, data.supervisor],
    ]);
    assert.equal(
      JSON.parse(readFileSync(result.config, "utf8")).python,
      python.new,
    );
    assert.equal(plistInterpreter(result.plist), python.new);
  } finally {
    rmSync(data.root, { recursive: true, force: true });
  }
});

test("a registered recovery job with an interpreter that cannot run recover_backend.py is reloaded", async () => {
  const data = fixture();
  const python = interpreters(data);
  const env = {
    ...data.env,
    PATH: `${path.dirname(python.old)}:${path.dirname(python.new)}`,
  };
  const calls = [];
  try {
    // The first registration saved `old` (the defect of 2026-10-24); launchd runs it.
    const initial = await configureRecovery({
      ...data,
      env,
      enabled: true,
      run: async () => {},
    });
    assert.equal(plistInterpreter(initial.plist), python.old);
    const result = await configureRecovery({
      ...data,
      env,
      enabled: true,
      checkInterpreter: async (candidate) => {
        if (candidate === python.old)
          throw new Error("ImportError: cannot import name 'StrEnum'");
      },
      run: async (_file, args) => {
        calls.push(args[0]);
        return { stdout: "state = running\n" };
      },
    });
    assert.deepEqual(calls, ["print", "bootout", "bootstrap", "print"]);
    assert.equal(plistInterpreter(result.plist), python.new);
    assert.equal(
      JSON.parse(readFileSync(result.config, "utf8")).python,
      python.new,
    );
  } finally {
    rmSync(data.root, { recursive: true, force: true });
  }
});

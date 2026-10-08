const fs = require("node:fs");
const path = require("node:path");
const os = require("node:os");
const { createHash } = require("node:crypto");
const { execFile } = require("node:child_process");
const { promisify } = require("node:util");
const installedApplicationPath =
  "/Applications/Codex Studio.app/Contents/Resources/app.asar";
const { executable, stateDirectory } = require("./backend.cjs");
const runFile = promisify(execFile);
const restartKeys = [
  "CODEX_HOME",
  "CODEX_CANVAS_CWD",
  "CODEX_CANVAS_CONCURRENCY",
  "CODEX_BIN",
  "CODEX_AGENTS_PYTHON",
  "SHELL",
  "LANG",
  "LC_ALL",
  "CODEX_AGENTS_SUPERVISOR_MODE",
  "CODEX_AGENTS_SUPERVISOR_FALLBACK",
];

function atomicJSON(filename, data) {
  fs.mkdirSync(path.dirname(filename), { recursive: true });
  const temporary = `${filename}.${process.pid}.tmp`;
  const fd = fs.openSync(temporary, "w", 0o600);
  try {
    fs.writeFileSync(fd, `${JSON.stringify(data, null, 2)}\n`);
    fs.fsyncSync(fd);
  } finally {
    fs.closeSync(fd);
  }
  fs.renameSync(temporary, filename);
  const dir = fs.openSync(path.dirname(filename), "r");
  try {
    fs.fsyncSync(dir);
  } finally {
    fs.closeSync(dir);
  }
}
function recoveryPaths(env = process.env, home = os.homedir()) {
  const state = stateDirectory(env);
  fs.mkdirSync(state, { recursive: true });
  const canonical = fs.realpathSync(state);
  const suffix = createHash("sha256")
    .update(canonical)
    .digest("hex")
    .slice(0, 16);
  return {
    state: canonical,
    config: path.join(canonical, "background-recovery.json"),
    label: `local.codex.agents.recovery.${suffix}`,
    plist: path.join(
      home,
      "Library/LaunchAgents",
      `local.codex.agents.recovery.${suffix}.plist`,
    ),
    supervisorLabel: `local.codex.agents.supervisor.${suffix}`,
    supervisorPlist: path.join(
      home,
      "Library/LaunchAgents",
      `local.codex.agents.supervisor.${suffix}.plist`,
    ),
  };
}
function recoveryPreference(env = process.env) {
  const { config } = recoveryPaths(env);
  try {
    return JSON.parse(fs.readFileSync(config, "utf8")).enabled === true;
  } catch (error) {
    if (error.code === "ENOENT") return true;
    throw error;
  }
}
function supervisorPreference(env = process.env) {
  if (env.CODEX_AGENTS_SUPERVISOR_MODE !== undefined)
    return env.CODEX_AGENTS_SUPERVISOR_MODE === "1";
  const { config } = recoveryPaths(env);
  try {
    const saved = JSON.parse(fs.readFileSync(config, "utf8"));
    if (
      saved.supervisorEnabled !== undefined &&
      typeof saved.supervisorEnabled !== "boolean"
    )
      throw new Error("The saved supervisor setting is invalid.");
    return saved.supervisorEnabled === true;
  } catch (error) {
    if (error.code === "ENOENT") return false;
    throw error;
  }
}
function xml(value) {
  return String(value).replace(
    /[&<>"']/g,
    (char) =>
      ({
        "&": "&amp;",
        "<": "&lt;",
        ">": "&gt;",
        '"': "&quot;",
        "'": "&apos;",
      })[char],
  );
}
function launchAgent({ label, python, supervisor, config, state }) {
  // HTTP requests and native pipes need app resource limits. Adaptive needs XPC.
  const args = [python, "-B", supervisor, "--config", config];
  return `<?xml version="1.0" encoding="UTF-8"?>\n<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n<plist version="1.0"><dict>
<key>Label</key><string>${xml(label)}</string>
<key>ProgramArguments</key><array>${args.map((arg) => `<string>${xml(arg)}</string>`).join("")}</array>
<key>RunAtLoad</key><true/>
<key>KeepAlive</key><true/>
<key>ProcessType</key><string>Interactive</string>
<key>ThrottleInterval</key><integer>10</integer>
<key>LimitLoadToSessionType</key><string>Aqua</string>
<key>AbandonProcessGroup</key><true/>
<key>StandardOutPath</key><string>${xml(path.join(state, "background-recovery.log"))}</string>
<key>StandardErrorPath</key><string>${xml(path.join(state, "background-recovery.log"))}</string>
</dict></plist>\n`;
}
function supervisorLaunchAgent({ label, python, supervisor, state }) {
  const args = [python, "-B", supervisor, "--state", state, "--wait-for-lease"];
  return `<?xml version="1.0" encoding="UTF-8"?>\n<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n<plist version="1.0"><dict>
<key>Label</key><string>${xml(label)}</string>
<key>ProgramArguments</key><array>${args.map((arg) => `<string>${xml(arg)}</string>`).join("")}</array>
<key>RunAtLoad</key><true/>
<key>KeepAlive</key><true/>
<key>ProcessType</key><string>Interactive</string>
<key>ThrottleInterval</key><integer>10</integer>
<key>LimitLoadToSessionType</key><string>Aqua</string>
<key>AbandonProcessGroup</key><true/>
<key>StandardOutPath</key><string>${xml(path.join(state, "supervisor.log"))}</string>
<key>StandardErrorPath</key><string>${xml(path.join(state, "supervisor.log"))}</string>
</dict></plist>\n`;
}
async function assertRecoveryBootoutSafe({
  serviceInfo,
  files,
  python,
  resources,
  run,
}) {
  const serviceOutput =
    typeof serviceInfo?.stdout === "string" ? serviceInfo.stdout : "";
  const jobPid = Number(serviceOutput.match(/^\s*pid\s*=\s*(\d+)\s*$/m)?.[1]);
  if (!Number.isSafeInteger(jobPid) || jobPid <= 0) return;

  let owner;
  try {
    owner = JSON.parse(
      fs.readFileSync(path.join(files.state, "supervisor.lock"), "utf8"),
    );
  } catch (error) {
    if (error.code === "ENOENT") return;
    throw new Error(
      `Cannot verify the legacy supervisor owner before stopping recovery: ${error.message}`,
    );
  }
  if (
    !Number.isSafeInteger(owner.pid) ||
    owner.pid <= 0 ||
    typeof owner.startTime !== "string"
  )
    throw new Error(
      "Cannot verify the legacy supervisor identity before stopping recovery.",
    );
  let parentOutput;
  try {
    const result = await run("/bin/ps", [
      "-p",
      String(owner.pid),
      "-o",
      "ppid=",
    ]);
    parentOutput = typeof result?.stdout === "string" ? result.stdout : "";
  } catch (error) {
    if (error.code === 1 && !String(error.stdout || "").trim()) return;
    throw new Error(
      `Cannot verify whether the legacy supervisor belongs to recovery: ${error.message}`,
    );
  }
  if (!parentOutput.trim()) return;
  if (!Number.isSafeInteger(Number(parentOutput.trim())))
    throw new Error(
      "Cannot verify the legacy supervisor parent process before stopping recovery.",
    );
  if (Number(parentOutput.trim()) !== jobPid) return;

  let health;
  try {
    const script = path.resolve(
      resources,
      "scripts/codex_process_supervisor.py",
    );
    const result = await run(python, [
      "-B",
      script,
      "--status-json",
      "--state",
      files.state,
    ]);
    health = JSON.parse(result?.stdout || "");
  } catch (error) {
    throw new Error(
      `Cannot safely stop the recovery LaunchAgent while its legacy supervisor is live: ${error.message}`,
    );
  }
  if (!Array.isArray(health.handles))
    throw new Error(
      "Cannot safely stop the recovery LaunchAgent while its legacy supervisor status is unavailable.",
    );
  if (health.handles.length > 0)
    throw new Error(
      `Cannot stop the recovery LaunchAgent while legacy supervisor pid ${owner.pid} owns ${health.handles.length} live handle(s). Wait for the owner to exit or all handles to close.`,
    );
}
async function ensureSupervisorAgent({ files, python, resources, uid, run }) {
  const script = path.resolve(resources, "scripts/codex_process_supervisor.py");
  fs.accessSync(script, fs.constants.R_OK);
  fs.mkdirSync(path.dirname(files.supervisorPlist), { recursive: true });
  const nextPlist = supervisorLaunchAgent({
    label: files.supervisorLabel,
    python,
    supervisor: script,
    state: files.state,
  });
  const temporary = `${files.supervisorPlist}.${process.pid}.tmp`;
  fs.writeFileSync(temporary, nextPlist, { mode: 0o600 });
  fs.renameSync(temporary, files.supervisorPlist);

  const domain = `gui/${uid}`;
  const service = `${domain}/${files.supervisorLabel}`;
  let registered = true;
  try {
    await run("/bin/launchctl", ["print", service]);
  } catch {
    registered = false;
  }
  // Never unload or kickstart the owner: its child pipes carry live work.
  if (!registered)
    await run("/bin/launchctl", ["bootstrap", domain, files.supervisorPlist]);
  await run("/bin/launchctl", ["print", service]);
}
function isInstalledApplication(applicationPath) {
  return (
    typeof applicationPath === "string" &&
    path.resolve(applicationPath) === installedApplicationPath
  );
}
function recoveryStatusLabel(enabled, available = true) {
  if (!available) return "Background recovery: Unavailable";
  return `Background recovery: ${enabled ? "On" : "Off"}`;
}
async function configureRecovery({
  applicationPath,
  resources,
  supervisor,
  enabled,
  port = 4620,
  env = process.env,
  restartEnvironment,
  home = os.homedir(),
  uid = process.getuid(),
  run = runFile,
}) {
  if (!Number.isInteger(port) || port < 1024 || port > 65535)
    throw new Error("Invalid recovery port.");
  if (!isInstalledApplication(applicationPath))
    throw new Error(
      "Background recovery can be registered only by /Applications/Codex Studio.app.",
    );
  const files = recoveryPaths(env, home);
  const domain = `gui/${uid}`;
  const service = `${domain}/${files.label}`;
  const authoritative =
    restartEnvironment && typeof restartEnvironment === "object";
  const launchEnv = { ...env };
  if (authoritative) {
    for (const key of restartKeys) {
      delete launchEnv[key];
      if (typeof restartEnvironment[key] === "string")
        launchEnv[key] = restartEnvironment[key];
    }
  }
  let saved = {};
  try {
    saved = JSON.parse(fs.readFileSync(files.config, "utf8"));
  } catch (error) {
    if (error.code !== "ENOENT") throw error;
  }
  if (
    saved.supervisorEnabled !== undefined &&
    typeof saved.supervisorEnabled !== "boolean"
  )
    throw new Error("The saved supervisor setting is invalid.");
  const supervisorEnabled =
    launchEnv.CODEX_AGENTS_SUPERVISOR_MODE === undefined
      ? saved.supervisorEnabled === true
      : launchEnv.CODEX_AGENTS_SUPERVISOR_MODE === "1";
  const python =
    enabled || supervisorEnabled ? executable("python3", launchEnv) : null;
  const safetyPython = python || executable("python3", launchEnv);
  if (supervisorEnabled)
    await ensureSupervisorAgent({ files, python, resources, uid, run });
  if (!enabled) {
    let serviceInfo;
    try {
      serviceInfo = await run("/bin/launchctl", ["print", service]);
    } catch {
      atomicJSON(files.config, {
        ...saved,
        version: 1,
        stateDir: files.state,
        enabled: false,
        supervisorEnabled,
      });
      fs.rmSync(files.plist, { force: true });
      return { enabled: false, ...files };
    }
    await assertRecoveryBootoutSafe({
      serviceInfo,
      files,
      python: safetyPython,
      resources,
      run,
    });
    atomicJSON(files.config, {
      ...saved,
      version: 1,
      stateDir: files.state,
      enabled: false,
      supervisorEnabled,
    });
    await run("/bin/launchctl", ["bootout", service]);
    fs.rmSync(files.plist, { force: true });
    return { enabled: false, ...files };
  }
  const codex = executable("codex", launchEnv);
  for (const filename of [
    supervisor,
    path.join(resources, "scripts/codex-canvas"),
    path.join(resources, "web/dist/index.html"),
  ])
    fs.accessSync(filename, fs.constants.R_OK);
  const environment = {};
  for (const key of restartKeys)
    if (key !== "CODEX_AGENTS_SUPERVISOR_MODE" && launchEnv[key] !== undefined)
      environment[key] = launchEnv[key];
  environment.PATH = `${path.dirname(codex)}:${env.PATH || "/usr/bin:/bin"}`;
  const config = {
    version: 1,
    enabled: true,
    supervisorEnabled,
    stateDir: files.state,
    resources: path.resolve(resources),
    python,
    codex,
    port,
    environment,
    unsetEnvironment: restartKeys.filter(
      (key) => environment[key] === undefined,
    ),
  };
  let previousPlist;
  try {
    previousPlist = fs.readFileSync(files.plist, "utf8");
  } catch (error) {
    if (error.code !== "ENOENT") throw error;
  }
  const nextPlist = launchAgent({
    ...files,
    python,
    supervisor: path.resolve(supervisor),
  });
  let serviceInfo;
  let registered = true;
  try {
    serviceInfo = await run("/bin/launchctl", ["print", service]);
  } catch {
    registered = false;
  }
  // Save automatic interpreter and policy changes for the next registration.
  // The registered recovery process can keep its original interpreter.
  const retainedPythonPlist =
    !launchEnv.CODEX_AGENTS_PYTHON &&
    typeof saved.python === "string" &&
    path.isAbsolute(saved.python)
      ? launchAgent({
          ...files,
          python: saved.python,
          supervisor: path.resolve(supervisor),
        })
      : nextPlist;
  const compatiblePlists = [nextPlist, retainedPythonPlist].flatMap((plist) => [
    plist,
    plist.replace("<key>ProcessType</key><string>Interactive</string>\n", ""),
  ]);
  const restartRecovery =
    registered &&
    previousPlist !== undefined &&
    !compatiblePlists.includes(previousPlist);
  if (restartRecovery)
    await assertRecoveryBootoutSafe({
      serviceInfo,
      files,
      python: safetyPython,
      resources,
      run,
    });
  atomicJSON(files.config, config);
  fs.mkdirSync(path.dirname(files.plist), { recursive: true });
  const temporary = `${files.plist}.${process.pid}.tmp`;
  fs.writeFileSync(temporary, nextPlist, { mode: 0o600 });
  fs.renameSync(temporary, files.plist);
  if (restartRecovery) {
    // Restart only recovery after proving it does not own live supervisor work.
    await run("/bin/launchctl", ["bootout", service]);
    registered = false;
  }
  if (!registered) {
    try {
      await run("/bin/launchctl", ["bootstrap", domain, files.plist]);
    } catch (error) {
      try {
        // A failed response does not prove that registration failed.
        await run("/bin/launchctl", ["print", service]);
      } catch (statusError) {
        if (!restartRecovery || statusError.code !== 113) throw error;
        fs.writeFileSync(temporary, previousPlist, { mode: 0o600 });
        fs.renameSync(temporary, files.plist);
        try {
          await run("/bin/launchctl", ["bootstrap", domain, files.plist]);
          await run("/bin/launchctl", ["print", service]);
        } catch (restoreError) {
          throw new Error(
            `Cannot update or restore background recovery: ${restoreError.message}`,
            { cause: error },
          );
        }
        throw new Error(
          "Cannot update background recovery. The previous recovery service was restored.",
          { cause: error },
        );
      }
    }
  }
  // Read back launchd registration. Writing a plist alone does not install a service.
  await run("/bin/launchctl", ["print", service]);
  return { enabled: true, ...files };
}
module.exports = {
  atomicJSON,
  configureRecovery,
  recoveryPreference,
  supervisorPreference,
  recoveryPaths,
  launchAgent,
  supervisorLaunchAgent,
  isInstalledApplication,
  recoveryStatusLabel,
};

function trackDesktopRecovery({ app, env = process.env }) {
  const { state } = recoveryPaths(env);
  const filename = path.join(state, "desktop-recovery.json");
  const { execFileSync } = require("node:child_process");
  const startedAt = execFileSync(
    "/bin/ps",
    ["-p", String(process.pid), "-o", "lstart="],
    { encoding: "utf8", env: { ...process.env, LC_ALL: "C" } },
  ).trim();
  if (!startedAt) throw new Error("Cannot identify the desktop process.");
  const intent = {
    version: 1,
    desiredOpen: true,
    pid: process.pid,
    startedAt,
    executable: process.execPath,
    profile: app.getPath("userData"),
  };
  atomicJSON(filename, intent);
  const closeExplicitly = () => {
    try {
      atomicJSON(filename, { ...intent, desiredOpen: false });
    } catch (error) {
      // A missing intent is safer than reopening after an unsaved explicit Quit.
      try {
        fs.unlinkSync(filename);
      } catch {}
      console.error("Cannot save desktop close intent:", error.message);
    }
  };
  // Electron does not identify every macOS Quit reason. A graceful exit always
  // withdraws the open request; macOS owns its normal reopen-windows preference.
  app.on("before-quit", closeExplicitly);
  app.on("window-all-closed", closeExplicitly);
  return { filename, closeExplicitly, windowClosing: closeExplicitly };
}
module.exports.trackDesktopRecovery = trackDesktopRecovery;

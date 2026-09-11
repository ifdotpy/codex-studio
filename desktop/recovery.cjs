const fs = require("node:fs");
const path = require("node:path");
const os = require("node:os");
const { createHash } = require("node:crypto");
const { execFile } = require("node:child_process");
const { promisify } = require("node:util");
const { executable, stateDirectory } = require("./backend.cjs");
const runFile = promisify(execFile);
const restartKeys = [
  "CODEX_HOME",
  "CODEX_BOARD_STATE_DIR",
  "CODEX_CANVAS_CWD",
  "CODEX_CANVAS_CONCURRENCY",
  "CODEX_BIN",
  "SHELL",
  "LANG",
  "LC_ALL",
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
  const args = [python, "-B", supervisor, "--config", config];
  return `<?xml version="1.0" encoding="UTF-8"?>\n<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n<plist version="1.0"><dict>
<key>Label</key><string>${xml(label)}</string>
<key>ProgramArguments</key><array>${args.map((arg) => `<string>${xml(arg)}</string>`).join("")}</array>
<key>RunAtLoad</key><true/>
<key>KeepAlive</key><true/>
<key>ThrottleInterval</key><integer>10</integer>
<key>LimitLoadToSessionType</key><string>Aqua</string>
<key>AbandonProcessGroup</key><true/>
<key>StandardOutPath</key><string>${xml(path.join(state, "background-recovery.log"))}</string>
<key>StandardErrorPath</key><string>${xml(path.join(state, "background-recovery.log"))}</string>
</dict></plist>\n`;
}
async function configureRecovery({
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
  const files = recoveryPaths(env, home);
  const domain = `gui/${uid}`;
  const service = `${domain}/${files.label}`;
  if (!enabled) {
    let saved = {};
    try {
      saved = JSON.parse(fs.readFileSync(files.config, "utf8"));
    } catch (error) {
      if (error.code !== "ENOENT") throw error;
    }
    atomicJSON(files.config, {
      ...saved,
      version: 1,
      stateDir: files.state,
      enabled: false,
    });
    try {
      await run("/bin/launchctl", ["print", service]);
    } catch {
      fs.rmSync(files.plist, { force: true });
      return { enabled: false, ...files };
    }
    await run("/bin/launchctl", ["bootout", service]);
    fs.rmSync(files.plist, { force: true });
    return { enabled: false, ...files };
  }
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
  const python = executable("python3", launchEnv);
  const codex = executable("codex", launchEnv);
  for (const filename of [
    supervisor,
    path.join(resources, "scripts/codex-canvas"),
    path.join(resources, "web/dist/index.html"),
  ])
    fs.accessSync(filename, fs.constants.R_OK);
  const environment = {};
  for (const key of restartKeys)
    if (launchEnv[key] !== undefined) environment[key] = launchEnv[key];
  environment.PATH = `${path.dirname(codex)}:${env.PATH || "/usr/bin:/bin"}`;
  atomicJSON(files.config, {
    version: 1,
    enabled: true,
    stateDir: files.state,
    resources: path.resolve(resources),
    python,
    codex,
    port,
    environment,
    unsetEnvironment: restartKeys.filter(
      (key) => environment[key] === undefined,
    ),
  });
  fs.mkdirSync(path.dirname(files.plist), { recursive: true });
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
  const temporary = `${files.plist}.${process.pid}.tmp`;
  fs.writeFileSync(temporary, nextPlist, { mode: 0o600 });
  fs.renameSync(temporary, files.plist);
  let registered = true;
  try {
    await run("/bin/launchctl", ["print", service]);
  } catch {
    registered = false;
  }
  if (
    registered &&
    previousPlist !== undefined &&
    previousPlist !== nextPlist
  ) {
    // Restart only the supervisor when an application or Python path changes.
    // The detached backend keeps its authoritative lease and active work.
    await run("/bin/launchctl", ["bootout", service]);
    registered = false;
  }
  if (!registered)
    await run("/bin/launchctl", ["bootstrap", domain, files.plist]);
  // Read back launchd registration. Writing a plist alone does not install a service.
  await run("/bin/launchctl", ["print", service]);
  return { enabled: true, ...files };
}
module.exports = {
  atomicJSON,
  configureRecovery,
  recoveryPreference,
  recoveryPaths,
  launchAgent,
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

if (process.argv.includes("--render-panel")) {
  require("./panel-renderer.cjs");
  return;
}
const {
  app,
  BrowserWindow,
  ipcMain,
  dialog,
  shell,
  Notification,
  Menu,
  systemPreferences,
  screen,
} = require("electron");
const fs = require("node:fs/promises");
const path = require("node:path");
const { ensureBackend, identity, updateStatus } = require("./backend.cjs");
const {
  configureRecovery,
  recoveryPreference,
  trackDesktopRecovery,
} = require("./recovery.cjs");
const { loadWindowState, trackWindowState } = require("./window-state.cjs");
const hidden = process.argv.includes("--hidden");
const backgroundRecovery = process.argv.includes("--background-recovery");
let recoveryEnabled = false;
let desktopRecovery;
const recoverySupported =
  app.isPackaged && process.platform === "darwin" && !hidden;
async function setBackgroundRecovery(enabled) {
  const result = await configureRecovery({
    resources: backendResources,
    supervisor: path.join(process.resourcesPath, "recover_backend.py"),
    port: Number(process.env.CODEX_DESKTOP_PORT || 4620),
    enabled,
    restartEnvironment: backend?.restartEnvironment,
  });
  recoveryEnabled = result.enabled;
}
// Preserve the existing browser profile when the product name changes.
app.setPath(
  "userData",
  process.env.CODEX_DESKTOP_PROFILE ||
    path.join(app.getPath("appData"), "Codex Agents"),
);
app.setName("Codex Studio");
app.enableSandbox();
let win;
let backend;
let backendResources;
let notifications = false;
let microphoneUntil = 0;
const transcriptionPermits = new Map();
let transcriptionRunning = null;
let notificationWrite = Promise.resolve();
const notificationSettings = path.join(
  app.getPath("userData"),
  "notifications.json",
);
async function loadNotifications() {
  try {
    notifications =
      JSON.parse(await fs.readFile(notificationSettings, "utf8")).enabled ===
      true;
  } catch (error) {
    if (error.code !== "ENOENT")
      console.error("Cannot read notification settings:", error.message);
  }
}
function notificationTarget(value) {
  if (!value || value.section !== "messages")
    throw new Error("Invalid notification target.");
  return {
    agentId: string(value.agentId, 512),
    section: "messages",
    ...(value.itemId === undefined
      ? {}
      : { itemId: string(value.itemId, 512) }),
  };
}
function trusted(event) {
  if (
    !win ||
    event.sender !== win.webContents ||
    event.senderFrame !== win.webContents.mainFrame ||
    event.senderFrame.url !== `${backend.origin}/`
  )
    throw new Error("Native access is restricted to the workspace main frame.");
}
function string(value, max = 4096) {
  if (
    typeof value !== "string" ||
    !value.length ||
    value.length > max ||
    value.includes("\0")
  )
    throw new Error("Invalid native action value.");
  return value;
}
function externalURL(value) {
  const url = new URL(string(value));
  if (
    !["https:", "http:"].includes(url.protocol) ||
    url.username ||
    url.password
  )
    throw new Error("Only HTTP and HTTPS links are supported.");
  return url.href;
}
function mime(file) {
  return (
    {
      ".png": "image/png",
      ".jpg": "image/jpeg",
      ".jpeg": "image/jpeg",
      ".webp": "image/webp",
      ".gif": "image/gif",
      ".svg": "image/svg+xml",
      ".pdf": "application/pdf",
      ".txt": "text/plain",
      ".md": "text/markdown",
      ".html": "text/html",
      ".json": "application/json",
    }[path.extname(file).toLowerCase()] || "application/octet-stream"
  );
}
async function nativeAction(event, request) {
  trusted(event);
  if (!request || typeof request !== "object")
    throw new Error("Invalid native action.");
  switch (request.method) {
    case "requestMicrophone": {
      if (
        process.platform === "darwin" &&
        !(await systemPreferences.askForMediaAccess("microphone"))
      )
        throw new Error(
          "Microphone access was denied. Allow Codex Studio in macOS Privacy & Security.",
        );
      microphoneUntil = Date.now() + 15000;
      return true;
    }
    case "prepareTranscription": {
      if (process.platform !== "darwin")
        throw new Error("Local transcription currently requires macOS.");
      for (const [token, permit] of transcriptionPermits)
        if (permit.expiry < Date.now()) transcriptionPermits.delete(token);
      const token = require("node:crypto").randomUUID();
      transcriptionPermits.set(token, {
        expiry: Date.now() + 60000,
        sender: event.sender,
      });
      return token;
    }
    case "transcribeAudio": {
      const token = request.value?.permit;
      const permit = transcriptionPermits.get(token);
      transcriptionPermits.delete(token);
      if (
        !permit ||
        permit.expiry < Date.now() ||
        permit.sender !== event.sender
      )
        throw new Error("Use the Transcribe button again.");
      if (transcriptionRunning)
        throw new Error(
          "Another recording is being transcribed. Retry when it finishes.",
        );
      const id = string(request.value?.id, 128);
      const controller = new AbortController();
      const sender = event.sender;
      transcriptionRunning = { id, sender, controller };
      const abort = () => controller.abort();
      const navigate = (_event, _url, _inPlace, isMainFrame) => {
        if (isMainFrame) abort();
      };
      sender.once("destroyed", abort);
      sender.on("did-start-navigation", navigate);
      try {
        const helper = app.isPackaged
          ? path.join(process.resourcesPath, "studio-speech")
          : path.join(__dirname, "native", "studio-speech");
        return await require("./speech.cjs").transcribe(request.value, helper, {
          signal: controller.signal,
          onProgress: (progress) => {
            if (!controller.signal.aborted && !sender.isDestroyed())
              sender.send("codex-desktop-transcription-progress", {
                id,
                ...progress,
              });
          },
        });
      } finally {
        sender.removeListener("destroyed", abort);
        sender.removeListener("did-start-navigation", navigate);
        transcriptionRunning = null;
      }
    }
    case "cancelTranscription": {
      const id = string(request.value, 128);
      if (
        !transcriptionRunning ||
        transcriptionRunning.id !== id ||
        transcriptionRunning.sender !== event.sender
      )
        return false;
      transcriptionRunning.controller.abort();
      return true;
    }
    case "pickDirectory": {
      const result = await dialog.showOpenDialog(win, {
        properties: ["openDirectory", "createDirectory"],
      });
      return result.canceled ? null : result.filePaths[0];
    }
    case "pickFiles": {
      const result = await dialog.showOpenDialog(win, {
        properties: ["openFile", "multiSelections"],
      });
      if (result.canceled) return [];
      if (result.filePaths.length > 20)
        throw new Error("Select at most 20 files.");
      let size = 0;
      const files = [];
      for (const file of result.filePaths) {
        const stat = await fs.stat(file);
        size += stat.size;
        if (!stat.isFile() || size > 20 * 1024 * 1024)
          throw new Error(
            "Select regular files with a total size of at most 20 MB.",
          );
        files.push({
          name: path.basename(file),
          path: file,
          mime: mime(file),
          data: (await fs.readFile(file)).toString("base64"),
        });
      }
      return files;
    }
    case "revealPath": {
      const value = string(request.value);
      if (!path.isAbsolute(value))
        throw new Error("A local absolute path is required.");
      await fs.stat(value);
      shell.showItemInFolder(value);
      return;
    }
    case "openExternal":
      await shell.openExternal(externalURL(request.value), { activate: false });
      return;
    case "getBackendUpdate": {
      const running = await identity(backend.origin, backend.stateDir);
      if (!running) throw new Error("The local backend is unavailable.");
      return updateStatus(backendResources, running);
    }
    case "getNotifications":
      await notificationWrite;
      return notifications && Notification.isSupported();
    case "setNotifications": {
      if (typeof request.value !== "boolean")
        throw new Error("Notification permission must be true or false.");
      const enabled = request.value;
      const write = notificationWrite
        .catch(() => {})
        .then(async () => {
          await fs.mkdir(path.dirname(notificationSettings), {
            recursive: true,
          });
          const temporary = `${notificationSettings}.tmp`;
          await fs.writeFile(temporary, JSON.stringify({ enabled }), {
            mode: 0o600,
          });
          await fs.rename(temporary, notificationSettings);
          notifications = enabled;
        });
      notificationWrite = write;
      await write;
      return notifications && Notification.isSupported();
    }
    case "notify": {
      if (!notifications || !Notification.isSupported()) return false;
      const target = notificationTarget(request.value?.target);
      const notification = new Notification({
        title: string(request.value?.title, 160),
        body: string(request.value?.body, 2000),
        silent: true,
      });
      notification.on("click", () => {
        if (
          !win ||
          win.isDestroyed() ||
          win.webContents.getURL() !== `${backend.origin}/`
        )
          return;
        if (win.isMinimized()) win.restore();
        win.show();
        win.focus();
        win.webContents.send("codex-desktop-navigate", target);
      });
      notification.show();
      return true;
    }
    default:
      throw new Error("Unknown native action.");
  }
}
async function start() {
  if (recoverySupported) {
    try {
      desktopRecovery = trackDesktopRecovery({ app });
    } catch (error) {
      console.error("Desktop recovery is unavailable:", error.message);
    }
  }
  await loadNotifications();
  backendResources = app.isPackaged
    ? path.join(process.resourcesPath, "workspace")
    : path.resolve(__dirname, "..");
  backend = await ensureBackend({
    resources: backendResources,
    port: Number(process.env.CODEX_DESKTOP_PORT || 4620),
  });
  if (recoverySupported) {
    try {
      await setBackgroundRecovery(recoveryPreference());
    } catch (error) {
      console.error("Background recovery is unavailable:", error.message);
    }
  }
  const primaryDisplay = screen.getPrimaryDisplay();
  const restoredWindow = loadWindowState(app.getPath("userData"), [
    primaryDisplay,
    ...screen
      .getAllDisplays()
      .filter((display) => display.id !== primaryDisplay.id),
  ]);
  win = new BrowserWindow({
    ...restoredWindow.bounds,
    minWidth: restoredWindow.minWidth,
    minHeight: restoredWindow.minHeight,
    show: false,
    title: "Codex Studio",
    ...(process.platform === "darwin"
      ? { titleBarStyle: "hidden", trafficLightPosition: { x: 20, y: 12 } }
      : {}),
    backgroundColor: "#18181b",
    webPreferences: {
      preload: path.join(__dirname, "preload.cjs"),
      sandbox: true,
      contextIsolation: true,
      nodeIntegration: false,
      webSecurity: true,
      webviewTag: false,
      allowRunningInsecureContent: false,
      spellcheck: true,
    },
  });
  const windowState = trackWindowState(
    win,
    app,
    app.getPath("userData"),
    restoredWindow,
    { hidden },
  );
  win.webContents.session.setPermissionRequestHandler(
    (contents, permission, callback, details) =>
      callback(
        contents === win.webContents &&
          permission === "media" &&
          details.isMainFrame === true &&
          details.requestingUrl === `${backend.origin}/` &&
          microphoneUntil > Date.now() &&
          Array.isArray(details.mediaTypes) &&
          details.mediaTypes.length === 1 &&
          details.mediaTypes[0] === "audio",
      ),
  );
  win.webContents.session.setPermissionCheckHandler(
    (contents, permission, origin, details) =>
      contents === win.webContents &&
      permission === "media" &&
      origin === backend.origin &&
      details.isMainFrame === true &&
      details.mediaType === "audio" &&
      microphoneUntil > Date.now(),
  );
  win.on("close", () => desktopRecovery?.windowClosing());
  let rendererFailures = 0;
  win.webContents.on("render-process-gone", (_event, details) => {
    if (details.reason === "clean-exit" || ++rendererFailures > 3) return;
    setTimeout(() => {
      if (win && !win.isDestroyed())
        win.loadURL(`${backend.origin}/`).catch(console.error);
    }, 1000 * rendererFailures);
  });
  win.webContents.on("will-attach-webview", (event) => event.preventDefault());
  win.webContents.setWindowOpenHandler(() => ({ action: "deny" }));
  win.webContents.on("will-navigate", (event) => event.preventDefault());
  win.webContents.on("will-frame-navigate", (event) => event.preventDefault());
  win.webContents.on("will-redirect", (event) => event.preventDefault());
  ipcMain.handle("codex-desktop", nativeAction);
  Menu.setApplicationMenu(
    Menu.buildFromTemplate([
      {
        label: "Codex Studio",
        submenu: [
          { role: "about" },
          ...(recoverySupported
            ? [
                {
                  label: "Restore server after login or failure",
                  type: "checkbox",
                  checked: recoveryEnabled,
                  click: async (item) => {
                    try {
                      await setBackgroundRecovery(item.checked);
                    } catch (error) {
                      item.checked = recoveryEnabled;
                      dialog.showErrorBox(
                        "Cannot change background recovery",
                        error.message,
                      );
                    }
                  },
                },
              ]
            : []),
          { type: "separator" },
          {
            label: "Quit Codex Studio",
            accelerator: "CommandOrControl+Q",
            click: () => {
              desktopRecovery?.closeExplicitly();
              app.quit();
            },
          },
        ],
      },
      { role: "editMenu" },
      {
        label: "View",
        submenu: [
          { role: "reload" },
          { role: "resetZoom" },
          { role: "zoomIn" },
          { role: "zoomOut" },
          { role: "togglefullscreen" },
        ],
      },
    ]),
  );
  await win.loadURL(`${backend.origin}/`);
  windowState.restore();
  if (!hidden) win.showInactive();
  console.log(
    JSON.stringify({
      event: "desktop-ready",
      origin: backend.origin,
      backendPid: backend.pid,
      backendOwned: backend.owned,
      hidden,
    }),
  );
}
if (!app.requestSingleInstanceLock()) app.quit();
else
  app
    .whenReady()
    .then(() => {
      if (hidden && process.platform === "darwin")
        app.setActivationPolicy("prohibited");

      return start();
    })
    .catch((error) => {
      console.error(error.stack || error);
      if (!hidden && !backgroundRecovery)
        dialog.showErrorBox("Codex Studio cannot start", error.message);
      app.exit(1);
    });
app.on("window-all-closed", () => app.quit());
// The backend owns durable agents. Closing the desktop never terminates that process.
module.exports = { trusted, nativeAction, externalURL };

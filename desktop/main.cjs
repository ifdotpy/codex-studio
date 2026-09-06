const {
  app,
  BrowserWindow,
  ipcMain,
  dialog,
  shell,
  Notification,
  Menu,
} = require("electron");
const fs = require("node:fs/promises");
const path = require("node:path");
const { ensureBackend } = require("./backend.cjs");
const hidden = process.argv.includes("--hidden");
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
let notifications = false;
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
    case "setNotifications":
      if (typeof request.value !== "boolean")
        throw new Error("Notification permission must be true or false.");
      notifications = request.value;
      return notifications && Notification.isSupported();
    case "notify":
      if (!notifications || !Notification.isSupported()) return false;
      new Notification({
        title: string(request.value?.title, 160),
        body: string(request.value?.body, 2000),
        silent: true,
      }).show();
      return true;
    default:
      throw new Error("Unknown native action.");
  }
}
async function start() {
  backend = await ensureBackend({
    resources: app.isPackaged
      ? path.join(process.resourcesPath, "workspace")
      : path.resolve(__dirname, ".."),
    port: Number(process.env.CODEX_DESKTOP_PORT || 4620),
  });
  win = new BrowserWindow({
    width: 1440,
    height: 960,
    minWidth: 420,
    minHeight: 600,
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
  win.webContents.session.setPermissionRequestHandler(
    (_contents, _permission, callback) => callback(false),
  );
  win.webContents.session.setPermissionCheckHandler(() => false);
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
        submenu: [{ role: "about" }, { type: "separator" }, { role: "quit" }],
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
      if (!hidden)
        dialog.showErrorBox("Codex Studio cannot start", error.message);
      app.exit(1);
    });
app.on("window-all-closed", () => app.quit());
// The backend owns durable agents. Closing the desktop never terminates that process.
module.exports = { trusted, nativeAction, externalURL };

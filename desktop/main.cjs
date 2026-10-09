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
  safeStorage,
} = require("electron");
const fs = require("node:fs/promises");
const path = require("node:path");
const { randomUUID } = require("node:crypto");
const { ensureBackend, identity, updateStatus } = require("./backend.cjs");
const {
  configureRecovery,
  isInstalledApplication,
  recoveryPreference,
  recoveryPaths,
  supervisorPreference,
  recoveryStatusLabel,
  trackDesktopRecovery,
} = require("./recovery.cjs");
const { loadWindowState, trackWindowState } = require("./window-state.cjs");
const { createRendererRecovery } = require("./renderer-recovery.cjs");
const { frameOwner } = require("./frame-owner.cjs");
const { startUiHost } = require("./ui-host.cjs");
const { uiOnlyInstallation } = require("./install-mode.cjs");
const uiOnly = uiOnlyInstallation({ packaged: app.isPackaged });
const hidden = process.argv.includes("--hidden");
const backgroundRecovery = process.argv.includes("--background-recovery");
let recoveryEnabled = false;
let recoveryAvailable = true;
let recoveryStatusItem;
let desktopRecovery;
const recoverySupported =
  app.isPackaged && process.platform === "darwin" && !hidden && !uiOnly;
async function setBackgroundRecovery(enabled) {
  const result = await configureRecovery({
    applicationPath: app.getAppPath(),
    resources: backendResources,
    supervisor: path.join(process.resourcesPath, "recover_backend.py"),
    port: Number(process.env.CODEX_DESKTOP_PORT || 4620),
    enabled,
    restartEnvironment: backend?.supervisorFallback
      ? {
          ...backend.restartEnvironment,
          CODEX_AGENTS_SUPERVISOR_MODE: "1",
        }
      : backend?.restartEnvironment,
  });
  recoveryEnabled = result.enabled;
  recoveryAvailable = true;
  if (recoveryStatusItem)
    recoveryStatusItem.label = recoveryStatusLabel(recoveryEnabled);
}
function markBackgroundRecoveryUnavailable(error) {
  recoveryAvailable = false;
  if (recoveryStatusItem)
    recoveryStatusItem.label = recoveryStatusLabel(false, false);
  console.error("Background recovery is unavailable:", error.message);
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
let frameHost;
let serverCredentials;
const serverStreams = new Map();
const workspaceURL = () =>
  `${backend.origin}/${uiOnly ? "?studio-ui-only=1" : ""}`;
let microphoneUntil = 0;
let microphoneOwner;
const transcriptionPermits = new Map();
const activeNotifications = new Set();
let transcriptionRunning = null;
function releaseServerView(serverId) {
  for (const stream of serverStreams.values())
    if (stream.serverId === serverId) stream.controller.abort();
  for (const [token, permit] of transcriptionPermits)
    if (permit.serverId === serverId) transcriptionPermits.delete(token);
  if (transcriptionRunning?.serverId === serverId) transcriptionRunning.abort();
  if (microphoneOwner === serverId) microphoneUntil = 0;
}
function notificationTarget(value) {
  if (!value || value.section !== "messages")
    throw new Error("Invalid notification target.");
  return {
    ...(value.serverId ? { serverId: string(value.serverId, 128) } : {}),
    agentId: string(value.agentId, 512),
    section: "messages",
    ...(value.itemId === undefined
      ? {}
      : { itemId: string(value.itemId, 512) }),
  };
}
function trusted(event) {
  if (!win || win.isDestroyed() || event.sender !== win.webContents)
    throw new Error("Native access is restricted to this workspace.");
  if (event.senderFrame === win.webContents.mainFrame) {
    if (event.senderFrame.url !== workspaceURL())
      throw new Error(
        "Native access is restricted to the workspace main frame.",
      );
    return;
  }
  const owner = frameOwner(
    event.senderFrame?.url,
    frameHost?.origin,
    backend.origin,
  );
  if (
    !owner ||
    event.senderFrame.parent !== win.webContents.mainFrame ||
    !win.webContents.mainFrame.frames.includes(event.senderFrame) ||
    event.senderFrame.isDestroyed?.() ||
    (event.studioServerId && event.studioServerId !== owner)
  )
    throw new Error("Invalid server frame owner.");
  event.studioServerId = owner;
  return owner;
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
const MAX_SAVE_BYTES = 64 * 1024 * 1024;
const DOCUMENT_EXTENSIONS = new Set(
  ".txt .md .markdown .pdf .rtf .csv .tsv .json .xml .yaml .yml .toml .log .diff .patch .png .jpg .jpeg .gif .webp .avif .heic .heif .tif .tiff .bmp .svg .mp3 .m4a .wav .aac .ogg .flac .mp4 .m4v .mov .webm .doc .docx .xls .xlsx .ppt .pptx .odt .ods .odp .pages .numbers .key".split(
    " ",
  ),
);
function saveName(value) {
  const name = path.basename(string(value, 1024).replaceAll("\\", "/"));
  // Reject control characters in exported file names.
  // oxlint-disable-next-line no-control-regex
  if (name === "." || name === ".." || /[\x00-\x1f]/.test(name))
    throw new Error("Invalid file name.");
  return name;
}
async function resolvedFile(event, target) {
  if (
    !target ||
    typeof target !== "object" ||
    Object.keys(target).some(
      (key) => !["agent", "path", "asset"].includes(key),
    ) ||
    (target.asset ? target.path !== undefined : !target.agent || !target.path)
  )
    throw new Error("Select a workspace file or attachment.");
  const query = new URLSearchParams();
  for (const [key, value] of Object.entries(target))
    query.set(key, string(value));
  const response = await fetch(`${backend.origin}/api/file-info?${query}`, {
    signal: AbortSignal.timeout(15000),
    redirect: "error",
  });
  const info = await response.json();
  if (!response.ok || info.error)
    throw new Error(info.error || "Cannot resolve the selected file.");
  trusted(event);
  const file = string(info.path);
  if (
    !path.isAbsolute(file) ||
    (await fs.realpath(file)) !== file ||
    !(await fs.stat(file)).isFile()
  )
    throw new Error("The selected file is no longer available.");
  return { path: file, name: saveName(info.name) };
}
function nativeDownload(event, item, contents) {
  if (!win || win.isDestroyed()) {
    event.preventDefault();
    return;
  }
  const url = item.getURL();
  if (
    contents !== win.webContents ||
    contents.getURL() !== workspaceURL() ||
    !(
      url.startsWith(`blob:${backend.origin}/`) ||
      url.startsWith(workspaceURL())
    )
  ) {
    event.preventDefault();
    return;
  }
  try {
    item.setSaveDialogOptions({
      title: "Save As",
      defaultPath: path.join(
        app.getPath("downloads"),
        saveName(item.getFilename()),
      ),
      properties: ["createDirectory", "showOverwriteConfirmation"],
    });
  } catch (error) {
    event.preventDefault();
    dialog.showErrorBox("Cannot save file", error.message);
    return;
  }
  item.once("done", (_event, state) => {
    if (state !== "completed" && state !== "cancelled")
      dialog.showErrorBox(
        "Cannot save file",
        "The file transfer did not complete. Try Save As again.",
      );
  });
}
async function nativeAction(event, request) {
  const owner = trusted(event);
  if (!request || typeof request !== "object")
    throw new Error("Invalid native action.");
  if (
    owner &&
    !["serverCredentialAction", "serverNativeAction"].includes(request.method)
  ) {
    if (
      owner !== "local" &&
      ["pickDirectory", "revealPath", "fileAction"].includes(request.method)
    )
      throw new Error(
        "This file belongs to another computer. Use the server folder list or Save As.",
      );
    if (
      ![
        "requestMicrophone",
        "prepareTranscription",
        "transcribeAudio",
        "cancelTranscription",
        "pickDirectory",
        "pickFiles",
        "revealPath",
        "saveFile",
        "fileAction",
        "openExternal",
        "getBackendUpdate",
      ].includes(request.method)
    )
      throw new Error("Unknown server native action.");
    if (owner !== "local" && request.method === "getBackendUpdate")
      return { availableBackendBuild: null, updateRequired: null };
  }
  switch (request.method) {
    case "releaseServerView": {
      if (owner) throw new Error("Use the workspace shell.");
      const serverId = string(request.value, 128);
      releaseServerView(serverId);
      return;
    }
    case "serverCredentialAction": {
      const value = request.value;
      if (
        !value ||
        ![
          "pair",
          "request",
          "summary",
          "cancel",
          "forget",
          "pairAttempt",
        ].includes(value.action)
      )
        throw new Error("Invalid server credential action.");
      if (
        ["request", "cancel"].includes(value.action) &&
        (!owner ||
          value.serverId !== owner ||
          (value.frameOwner && value.frameOwner !== owner))
      )
        throw new Error("Invalid server frame owner.");
      if (owner && ["pair", "forget", "pairAttempt"].includes(value.action))
        throw new Error("Use the server manager.");
      if (value.action === "pairAttempt") {
        if (owner || value.frameOwner)
          throw new Error("Use the server manager.");
        return serverCredentials.hasPairAttempt({
          origin: value.origin,
          serverId: string(value.serverId, 128),
          requestId: string(value.requestId, 128),
          inviteId: value.inviteId ? string(value.inviteId, 128) : undefined,
        });
      }
      if (value.action === "summary") {
        if (
          owner ||
          value.frameOwner ||
          value.method !== "GET" ||
          value.body ||
          new URL(value.url).pathname !== "/api/ui-summary" ||
          new URL(value.url).search ||
          new URL(value.url).hash
        )
          throw new Error("Invalid shell summary request.");
        const controller = new AbortController();
        const deadline = setTimeout(() => controller.abort(), 15000);
        try {
          let response;
          if (value.serverId === "local") {
            if (uiOnly || new URL(value.url).origin !== backend.origin)
              throw new Error("The request does not belong to this server.");
            response = await fetch(value.url, {
              method: "GET",
              headers: value.headers,
              signal: controller.signal,
              redirect: "error",
            });
          } else
            response = await serverCredentials.request(
              value,
              controller.signal,
            );
          trusted(event);
          return {
            status: response.status,
            statusText: response.statusText,
            headers: [...response.headers],
            body: await response.arrayBuffer(),
          };
        } finally {
          clearTimeout(deadline);
        }
      }
      if (value.action === "pair") {
        if (value.frameOwner) throw new Error("Pair from the server manager.");
        return serverCredentials.pair(value);
      }
      if (value.action === "forget") {
        if (value.frameOwner)
          throw new Error("Remove from the server manager.");
        for (const stream of serverStreams.values())
          if (stream.serverId === value.serverId) stream.controller.abort();
        return serverCredentials.forget(value.serverId);
      }
      if (value.action === "request") {
        const path = decodeURIComponent(new URL(value.url).pathname).replace(
          /\/+$/,
          "",
        );
        if (
          (value.method === "POST" &&
            (path === "/api/multi-server" ||
              path.startsWith("/api/multi-server/"))) ||
          path.startsWith("/api/multi-server/v1/")
        )
          throw new Error("Use the workspace shell for server management.");
      }
      const id = string(value.streamId, 128);
      if (value.action === "cancel") {
        const stream = serverStreams.get(id);
        if (
          stream &&
          stream.serverId === value.serverId &&
          stream.frameOwner === owner
        )
          stream.controller.abort();
        return;
      }
      if (serverStreams.has(id))
        throw new Error("The server stream identity is already in use.");
      const controller = new AbortController();
      const pending = {
        serverId: owner,
        frameOwner: owner,
        controller,
      };
      serverStreams.set(id, pending);
      const deadline = setTimeout(() => controller.abort(), 15000);
      try {
        let response;
        if (owner === "local") {
          if (uiOnly)
            throw new Error("The local server is unavailable in UI-only mode.");
          const url = new URL(value.url);
          if (
            url.origin !== backend.origin ||
            !url.pathname.startsWith("/api/") ||
            !["GET", "POST", "HEAD"].includes(value.method)
          )
            throw new Error("The request does not belong to this server.");
          response = await fetch(url, {
            method: value.method,
            headers: value.headers,
            ...(value.body?.byteLength ? { body: value.body } : {}),
            signal: controller.signal,
            redirect: "error",
          });
        } else
          response = await serverCredentials.request(value, controller.signal);
        trusted(event);
        const metadata = {
          status: response.status,
          statusText: response.statusText,
          headers: [...response.headers],
        };
        if (
          response.body &&
          response.headers.get("Content-Type")?.startsWith("text/event-stream")
        ) {
          clearTimeout(deadline);
          void (async () => {
            const reader = response.body.getReader();
            let bytesSinceYield = 0;
            try {
              while (!controller.signal.aborted) {
                const chunk = await reader.read();
                if (chunk.done) break;
                trusted(event);
                event.senderFrame.send("codex-desktop-server-stream", {
                  serverId: value.serverId,
                  streamId: id,
                  bytes: chunk.value.buffer.slice(
                    chunk.value.byteOffset,
                    chunk.value.byteOffset + chunk.value.byteLength,
                  ),
                });
                bytesSinceYield += chunk.value.byteLength;
                if (bytesSinceYield >= 1024 * 1024) {
                  bytesSinceYield = 0;
                  await new Promise((resolve) => setImmediate(resolve));
                }
              }
              if (!win.isDestroyed())
                event.senderFrame.send("codex-desktop-server-stream", {
                  serverId: value.serverId,
                  streamId: id,
                  done: true,
                });
            } catch {
              if (!win.isDestroyed())
                event.senderFrame.send("codex-desktop-server-stream", {
                  serverId: value.serverId,
                  streamId: id,
                  error: "The server connection closed.",
                });
            } finally {
              controller.abort();
              await reader.cancel().catch(() => {});
              serverStreams.delete(id);
            }
          })();
          return { ...metadata, streamId: id };
        }
        const body = await response.arrayBuffer();
        clearTimeout(deadline);
        trusted(event);
        serverStreams.delete(id);
        return { ...metadata, body };
      } catch (error) {
        clearTimeout(deadline);
        controller.abort();
        serverStreams.delete(id);
        throw error;
      }
    }
    case "serverNativeAction": {
      if (!owner || request.value?.serverId !== owner)
        throw new Error("Invalid server frame owner.");
      const method = string(request.value?.method, 128);
      const allowed = new Set([
        "requestMicrophone",
        "prepareTranscription",
        "transcribeAudio",
        "cancelTranscription",
        "pickDirectory",
        "pickFiles",
        "revealPath",
        "saveFile",
        "fileAction",
        "openExternal",
        "getBackendUpdate",
      ]);
      if (!allowed.has(method))
        throw new Error("Unknown server native action.");
      return nativeAction(event, { method, value: request.value.value });
    }

    case "saveFile": {
      const name = saveName(request.value?.name);
      const data = request.value?.data;
      if (
        !(data instanceof ArrayBuffer) ||
        !Number.isSafeInteger(data.byteLength) ||
        data.byteLength > MAX_SAVE_BYTES
      )
        throw new Error("Save accepts at most 64 MiB of file data.");
      const result = await dialog.showSaveDialog(win, {
        title: "Save As",
        defaultPath: path.join(app.getPath("downloads"), name),
        properties: ["createDirectory", "showOverwriteConfirmation"],
      });
      if (result.canceled || !result.filePath) return false;
      trusted(event);
      const temporary = path.join(
        path.dirname(result.filePath),
        `.studio-save-${randomUUID()}.tmp`,
      );
      try {
        await fs.writeFile(temporary, Buffer.from(data), {
          flag: "wx",
          mode: 0o600,
          flush: true,
        });
        trusted(event);
        await fs.rename(temporary, result.filePath);
      } catch (error) {
        await fs.rm(temporary, { force: true }).catch(() => {});
        throw error;
      }
      return true;
    }
    case "fileAction": {
      const action = request.value?.action;
      if (!["open", "reveal", "preview"].includes(action))
        throw new Error("Unknown file action.");
      const file = await resolvedFile(event, request.value.target);
      trusted(event);
      if (action === "reveal") {
        shell.showItemInFolder(file.path);
      } else if (action === "preview") {
        if (
          process.platform !== "darwin" ||
          typeof win.previewFile !== "function"
        )
          return false;
        win.previewFile(file.path, file.name);
      } else {
        if (
          !DOCUMENT_EXTENSIONS.has(path.extname(file.path).toLowerCase()) ||
          ((await fs.stat(file.path)).mode & 0o111) !== 0
        )
          throw new Error(
            "Use Quick Look or Show in Folder for this file type.",
          );
        trusted(event);
        const error = await shell.openPath(file.path);
        if (error) throw new Error(error);
      }
      return true;
    }
    case "requestMicrophone": {
      if (
        process.platform === "darwin" &&
        !(await systemPreferences.askForMediaAccess("microphone"))
      )
        throw new Error(
          "Microphone access was denied. Allow Codex Studio in macOS Privacy & Security.",
        );
      trusted(event);
      microphoneUntil = Date.now() + 15000;
      microphoneOwner = event.studioServerId;
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
        serverId: event.studioServerId,
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
        permit.sender !== event.sender ||
        permit.serverId !== event.studioServerId
      )
        throw new Error("Use the Transcribe button again.");
      if (transcriptionRunning)
        throw new Error(
          "Another recording is being transcribed. Retry when it finishes.",
        );
      const id = string(request.value?.id, 128);
      const controller = new AbortController();
      const sender = event.sender;
      const frame = event.senderFrame;
      const running = {
        id,
        sender,
        controller,
        serverId: event.studioServerId,
        abort: () => {},
      };
      transcriptionRunning = running;
      const cleanup = () => {
        sender.removeListener("destroyed", abort);
        sender.removeListener("did-start-navigation", navigate);
        clearInterval(liveness);
      };
      const abort = () => {
        controller.abort();
        cleanup();
      };
      running.abort = abort;
      const navigate = (
        details,
        _url,
        _inPlace,
        isMainFrame,
        processId,
        routingId,
      ) => {
        if (
          details.isMainFrame ||
          isMainFrame ||
          details.frame === frame ||
          (processId === frame.processId && routingId === frame.routingId)
        )
          abort();
      };
      const liveness = setInterval(() => {
        if (
          frame.isDestroyed?.() ||
          (frame !== sender.mainFrame &&
            !sender.mainFrame.frames.includes(frame))
        )
          abort();
      }, 1000);
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
              event.senderFrame.send("codex-desktop-transcription-progress", {
                id,
                ...progress,
              });
          },
        });
      } finally {
        cleanup();
        if (transcriptionRunning === running) transcriptionRunning = null;
      }
    }
    case "cancelTranscription": {
      const id = string(request.value, 128);
      if (
        !transcriptionRunning ||
        transcriptionRunning.id !== id ||
        transcriptionRunning.sender !== event.sender ||
        transcriptionRunning.serverId !== event.studioServerId
      )
        return false;
      transcriptionRunning.controller.abort();
      return true;
    }
    case "pickDirectory": {
      const result = await dialog.showOpenDialog(win, {
        properties: ["openDirectory", "createDirectory"],
      });
      trusted(event);
      return result.canceled ? null : result.filePaths[0];
    }
    case "pickFiles": {
      const result = await dialog.showOpenDialog(win, {
        properties: ["openFile", "multiSelections"],
      });
      trusted(event);
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
        trusted(event);
        files.push({
          name: path.basename(file),
          path: file,
          mime: mime(file),
          data: (await fs.readFile(file)).toString("base64"),
        });
      }
      trusted(event);
      return files;
    }
    case "revealPath": {
      const value = string(request.value);
      if (!path.isAbsolute(value))
        throw new Error("A local absolute path is required.");
      await fs.stat(value);
      trusted(event);
      shell.showItemInFolder(value);
      return;
    }
    case "openExternal":
      await shell.openExternal(externalURL(request.value), { activate: false });
      return;
    case "getBackendUpdate": {
      if (uiOnly) return { availableBackendBuild: null, updateRequired: false };
      const running = await identity(backend.origin, backend.stateDir);
      if (!running) throw new Error("The local backend is unavailable.");
      trusted(event);
      return updateStatus(backendResources, running);
    }
    case "notify": {
      if (!Notification.isSupported()) return false;
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
          win.webContents.getURL() !== workspaceURL()
        )
          return;
        if (win.isMinimized()) win.restore();
        win.show();
        win.focus();
        win.webContents.send("codex-desktop-navigate", target);
      });
      activeNotifications.add(notification);
      notification.once("close", () =>
        activeNotifications.delete(notification),
      );
      return await new Promise((resolve, reject) => {
        const timer = setTimeout(() => {
          activeNotifications.delete(notification);
          reject(
            new Error(
              "macOS did not confirm the notification. Check Studio in System Settings > Notifications.",
            ),
          );
        }, 15000);
        notification.once("show", () => {
          clearTimeout(timer);
          resolve(true);
        });
        notification.once("failed", (_event, error) => {
          clearTimeout(timer);
          activeNotifications.delete(notification);
          reject(
            new Error(
              /not allowed/i.test(error || "")
                ? "STUDIO_NOTIFICATIONS_DENIED: Enable Allow notifications for Codex Studio in macOS System Settings > Notifications."
                : error || "macOS could not show the notification.",
            ),
          );
        });
        notification.show();
      });
    }
    default:
      throw new Error("Unknown native action.");
  }
}
async function start() {
  if (recoverySupported) {
    if (isInstalledApplication(app.getAppPath())) {
      try {
        desktopRecovery = trackDesktopRecovery({ app });
      } catch (error) {
        console.error("Desktop recovery is unavailable:", error.message);
      }
    } else {
      console.error(
        "Desktop recovery is unavailable: this is not the installed Studio application.",
      );
    }
  }
  backendResources = app.isPackaged
    ? path.join(process.resourcesPath, "workspace")
    : path.resolve(__dirname, "..");
  if (recoverySupported) {
    try {
      if (!isInstalledApplication(app.getAppPath()))
        throw new Error(
          "Background recovery can be registered only by /Applications/Codex Studio.app.",
        );
      // Read the verified owner before an attaching desktop can change its recovery environment.
      backend = await identity(
        `http://127.0.0.1:${Number(process.env.CODEX_DESKTOP_PORT || 4620)}`,
        recoveryPaths().state,
      );
      await setBackgroundRecovery(recoveryPreference());
    } catch (error) {
      markBackgroundRecoveryUnavailable(error);
    }
  }
  backend = uiOnly
    ? await startUiHost({
        resources: backendResources,
        port: Number(process.env.CODEX_UI_PORT || 4621),
      })
    : await ensureBackend({
        resources: backendResources,
        port: Number(process.env.CODEX_DESKTOP_PORT || 4620),
        env: {
          ...process.env,
          CODEX_AGENTS_SUPERVISOR_MODE: supervisorPreference() ? "1" : "0",
        },
      });
  frameHost = uiOnly
    ? backend
    : await startUiHost({ resources: backendResources, port: 0 });
  serverCredentials =
    require("./server-credentials.cjs").createServerCredentials({
      fetchRequest: (request) => fetch(request),
      profile: app.getPath("userData"),
      safeStorage,
    });
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
      nodeIntegrationInSubFrames: true,
      additionalArguments: [`--studio-view-origin=${frameHost.origin}`],
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
  const allowedFrame = (url, main) => {
    if (main) return url === workspaceURL();
    return (win.webContents.mainFrame.frames || []).some(
      (frame) =>
        frame.url === url &&
        !!frameOwner(frame.url, frameHost.origin, backend.origin),
    );
  };
  const audioOwner = (url, main) =>
    allowedFrame(url, main) &&
    (main
      ? microphoneOwner === undefined
      : new URL(url).searchParams.get("studio-server") === microphoneOwner);
  win.webContents.session.setPermissionRequestHandler(
    (contents, permission, callback, details) =>
      callback(
        contents === win.webContents &&
          ((permission === "clipboard-sanitized-write" &&
            allowedFrame(details.requestingUrl, details.isMainFrame)) ||
            (permission === "media" &&
              audioOwner(details.requestingUrl, details.isMainFrame) &&
              microphoneUntil > Date.now() &&
              Array.isArray(details.mediaTypes) &&
              details.mediaTypes.length === 1 &&
              details.mediaTypes[0] === "audio")),
      ),
  );
  win.webContents.session.setPermissionCheckHandler(
    (contents, permission, origin, details) =>
      contents === win.webContents &&
      (origin === backend.origin ||
        allowedFrame(details.requestingUrl, false)) &&
      ((permission === "clipboard-sanitized-write" &&
        allowedFrame(details.requestingUrl, details.isMainFrame)) ||
        (permission === "media" &&
          details.mediaType === "audio" &&
          microphoneUntil > Date.now() &&
          audioOwner(details.requestingUrl, details.isMainFrame))),
  );
  win.on("close", () => {
    desktopRecovery?.windowClosing();
    for (const stream of serverStreams.values()) stream.controller.abort();
  });
  const rendererRecovery = createRendererRecovery({
    win,
    workspaceURL: workspaceURL(),
    profile: app.getPath("userData"),
  });
  win.webContents.on("will-attach-webview", (event) => event.preventDefault());
  win.webContents.setWindowOpenHandler(() => ({ action: "deny" }));
  win.webContents.on("will-navigate", (event) =>
    rendererRecovery.handleNavigation(event),
  );
  win.webContents.on("will-frame-navigate", (event) => {
    let url;
    try {
      url = new URL(event.url);
    } catch {}
    if (
      event.isMainFrame === false &&
      !!frameOwner(url?.href, frameHost.origin, backend.origin) &&
      (!frameOwner(event.frame?.url, frameHost.origin, backend.origin) ||
        frameOwner(event.frame.url, frameHost.origin, backend.origin) ===
          frameOwner(url.href, frameHost.origin, backend.origin)) &&
      event.frame?.parent === win.webContents.mainFrame
    )
      return;
    rendererRecovery.handleNavigation(event);
  });
  win.webContents.on("will-redirect", (event) => event.preventDefault());
  win.webContents.session.on?.("will-download", nativeDownload);
  ipcMain.handle("codex-desktop", nativeAction);
  app.once("before-quit", () => void frameHost.close());
  const applicationMenu = Menu.buildFromTemplate([
    {
      label: "Codex Studio",
      submenu: [
        { role: "about" },
        ...(recoverySupported
          ? [
              {
                id: "background-recovery-status",
                label: recoveryStatusLabel(recoveryEnabled, recoveryAvailable),
                enabled: false,
              },
              {
                label: "Restore server after login or failure",
                type: "checkbox",
                checked: recoveryEnabled,
                click: async (item) => {
                  try {
                    await setBackgroundRecovery(item.checked);
                  } catch (error) {
                    item.checked = recoveryEnabled;
                    markBackgroundRecoveryUnavailable(error);
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
        {
          id: "reload-workspace",
          label: "Reload",
          accelerator: "CommandOrControl+R",
          click: () => {
            void rendererRecovery.reload();
          },
        },
        { role: "resetZoom" },
        { role: "zoomIn" },
        { role: "zoomOut" },
        { role: "togglefullscreen" },
      ],
    },
  ]);
  recoveryStatusItem = applicationMenu.getMenuItemById(
    "background-recovery-status",
  );
  Menu.setApplicationMenu(applicationMenu);
  await rendererRecovery.reload({ manual: false });
  windowState.restore();
  if (!hidden) win.showInactive();
  console.log(
    JSON.stringify({
      event: "desktop-ready",
      origin: backend.origin,
      backendPid: backend.pid,
      backendOwned: backend.owned,
      hidden,
      uiOnly,
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

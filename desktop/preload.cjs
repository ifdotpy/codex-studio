const { contextBridge, ipcRenderer } = require("electron");

{
  let gesture = 0;
  const owner = new URLSearchParams(location.search).get("studio-server");
  const capture = (event) => {
    if (event.isTrusted) gesture = performance.now() + 1200;
  };
  window.addEventListener("click", capture, true);
  window.addEventListener("keydown", capture, true);
  const invoke = (method, value, needsGesture = true) => {
    if (needsGesture) {
      if (gesture < performance.now())
        return Promise.reject(
          new Error("Use a desktop button or keyboard action."),
        );
      gesture = 0;
    }
    return ipcRenderer.invoke("codex-desktop", { method, value });
  };
  if (process.isMainFrame)
    window.addEventListener("DOMContentLoaded", () => {
      new MutationObserver((records) => {
        for (const record of records)
          for (const node of record.removedNodes) {
            const removed = [
              ...(node.querySelectorAll?.("iframe") || []),
              ...(node.tagName === "IFRAME" ? [node] : []),
            ];
            for (const frame of removed) {
              const serverId = new URL(frame.src).searchParams.get(
                "studio-server",
              );
              if (serverId)
                void ipcRenderer
                  .invoke("codex-desktop", {
                    method: "releaseServerView",
                    value: serverId,
                  })
                  .catch(() => {});
            }
          }
      }).observe(document.body, { childList: true, subtree: true });
    });
  const subscriptions = new Set();
  window.addEventListener(
    "pagehide",
    () => {
      for (const stop of subscriptions) stop();
      subscriptions.clear();
    },
    { once: true },
  );
  const subscribe = (channel, callback) => {
    if (typeof callback !== "function") throw new Error("Expected a callback.");
    const listener = (_event, value) => callback(value);
    ipcRenderer.on(channel, listener);
    const stop = () => {
      ipcRenderer.removeListener(channel, listener);
      subscriptions.delete(stop);
    };
    subscriptions.add(stop);
    return stop;
  };
  contextBridge.exposeInMainWorld(
    "codexDesktop",
    Object.freeze({
      platform: process.platform,
      serverViewOrigin: process.argv
        .find((value) => value.startsWith("--studio-view-origin="))
        ?.slice("--studio-view-origin=".length),
      serverNativeAction: (serverId, method, value) => {
        const needsGesture = !["transcribeAudio", "getBackendUpdate"].includes(
          method,
        );
        if (owner !== serverId)
          return Promise.reject(new Error("Use a button in this server view."));
        return invoke(
          "serverNativeAction",
          { serverId, method, value },
          needsGesture,
        );
      },
      serverCredentialAction: (value) =>
        invoke(
          "serverCredentialAction",
          value,
          value.action === "pair" || value.action === "forget",
        ),
      onServerStream: (callback) =>
        subscribe("codex-desktop-server-stream", callback),
      requestMicrophone: () => invoke("requestMicrophone"),
      prepareTranscription: () => invoke("prepareTranscription"),
      transcribeAudio: (value) => invoke("transcribeAudio", value, false),
      cancelTranscription: (id) => invoke("cancelTranscription", id),
      onTranscriptionProgress: (callback) =>
        subscribe("codex-desktop-transcription-progress", callback),
      onNavigate: (callback) => subscribe("codex-desktop-navigate", callback),
      getBackendUpdate: () => invoke("getBackendUpdate", undefined, false),
      pickDirectory: () => invoke("pickDirectory"),
      pickFiles: () => invoke("pickFiles"),
      revealPath: (value) => invoke("revealPath", value),
      saveFile: (value) => invoke("saveFile", value),
      fileAction: (value) => invoke("fileAction", value),
      openExternal: (value) => invoke("openExternal", value),
      notify: (value) => invoke("notify", value, false),
    }),
  );
}

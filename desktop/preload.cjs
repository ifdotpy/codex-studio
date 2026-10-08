const { contextBridge, ipcRenderer } = require("electron");

if (process.isMainFrame) {
  let gesture = 0;
  let gestureOwner;
  const capture = (event) => {
    if (event.isTrusted) {
      gesture = performance.now() + 1200;
      gestureOwner = undefined;
    }
  };
  window.addEventListener("click", capture, true);
  window.addEventListener("keydown", capture, true);
  const attachedFrames = new WeakSet();
  const attachFrames = () => {
    for (const frame of document.querySelectorAll("iframe")) {
      if (attachedFrames.has(frame)) continue;
      attachedFrames.add(frame);
      frame.addEventListener("load", () => {
        try {
          const url = new URL(frame.src);
          const owner = url.searchParams.get("studio-server");
          if (url.origin !== location.origin || !owner) return;
          const captureFrame = (event) => {
            if (event.isTrusted) {
              gesture = performance.now() + 1200;
              gestureOwner = owner;
            }
          };
          frame.contentDocument.addEventListener("click", captureFrame, true);
          frame.contentDocument.addEventListener("keydown", captureFrame, true);
        } catch {}
      });
    }
  };
  window.addEventListener("DOMContentLoaded", () => {
    attachFrames();
    new MutationObserver(attachFrames).observe(document.body, {
      childList: true,
      subtree: true,
    });
  });
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
  const subscribe = (channel, callback) => {
    if (typeof callback !== "function") throw new Error("Expected a callback.");
    const listener = (_event, value) => callback(value);
    ipcRenderer.on(channel, listener);
    return () => ipcRenderer.removeListener(channel, listener);
  };
  contextBridge.exposeInMainWorld(
    "codexDesktop",
    Object.freeze({
      platform: process.platform,
      serverNativeAction: (serverId, method, value) => {
        const needsGesture = !["transcribeAudio", "getBackendUpdate"].includes(
          method,
        );
        if (needsGesture && gestureOwner !== serverId)
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

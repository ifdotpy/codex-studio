const { contextBridge, ipcRenderer } = require("electron");

if (process.isMainFrame) {
  let gesture = 0;
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
  contextBridge.exposeInMainWorld(
    "codexDesktop",
    Object.freeze({
      platform: process.platform,
      pickDirectory: () => invoke("pickDirectory"),
      pickFiles: () => invoke("pickFiles"),
      revealPath: (value) => invoke("revealPath", value),
      openExternal: (value) => invoke("openExternal", value),
      setNotifications: (value) => invoke("setNotifications", value),
      notify: (value) => invoke("notify", value, false),
    }),
  );
}

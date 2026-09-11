const assert = require("node:assert/strict");
const { app, BrowserWindow } = require("electron");
require("./main.cjs");
const timer = setTimeout(() => {
  console.error("Renderer recovery timed out.");
  app.exit(1);
}, 20000);
const wait = setInterval(() => {
  const win = BrowserWindow.getAllWindows()[0];
  if (!win) return;
  clearInterval(wait);
  win.webContents.once("did-finish-load", async () => {
    try {
      const origin = `http://127.0.0.1:${process.env.CODEX_DESKTOP_PORT}`;
      const original = await (await fetch(`${origin}/api/desktop`)).json();
      const renderer = win.webContents.getOSProcessId();
      win.webContents.once("did-finish-load", async () => {
        try {
          const restored = await (await fetch(`${origin}/api/desktop`)).json();
          assert.equal(restored.pid, original.pid);
          assert.notEqual(win.webContents.getOSProcessId(), renderer);
          assert.equal(win.isVisible(), false);
          assert.equal(
            await win.webContents.executeJavaScript(
              "typeof window.codexDesktop.getBackendUpdate",
            ),
            "function",
          );
          console.log(
            JSON.stringify({
              rendererRecovered: true,
              backendPid: restored.pid,
            }),
          );
          clearTimeout(timer);
          app.exit(0);
        } catch (error) {
          console.error(error);
          app.exit(1);
        }
      });
      win.webContents.forcefullyCrashRenderer();
    } catch (error) {
      console.error(error);
      app.exit(1);
    }
  });
}, 20);

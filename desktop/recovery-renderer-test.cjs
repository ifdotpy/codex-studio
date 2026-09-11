const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { app, Menu } = require("electron");
const timer = setTimeout(() => {
  console.error("Renderer recovery timed out.");
  app.exit(1);
}, 110000);
const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const loaded = (contents, action) =>
  new Promise((resolve) => {
    contents.once("did-finish-load", resolve);
    action();
  });
require("./main.cjs");
app.once("browser-window-created", (_event, win) => {
  const contents = win.webContents;
  contents.once("did-finish-load", async () => {
    try {
      const origin = `http://127.0.0.1:${process.env.CODEX_DESKTOP_PORT}`;
      const original = await (await fetch(`${origin}/api/desktop`)).json();
      const filename = path.join(
        app.getPath("userData"),
        "renderer-recovery.jsonl",
      );
      const records = () =>
        fs.readFileSync(filename, "utf8").trim().split("\n").map(JSON.parse);
      for (let attempt = 0; attempt < 4; attempt++) {
        await loaded(contents, () => contents.forcefullyCrashRenderer());
        assert.equal(win.isVisible(), false);
        assert.equal(contents.getURL() === `${origin}/`, attempt < 3);
      }
      assert.match(contents.getURL(), /^data:text\/html/);
      assert.equal(
        await contents.executeJavaScript(
          "document.querySelector('h1').textContent",
        ),
        "Studio needs to reload",
      );
      assert.equal(
        await contents.executeJavaScript(
          "window.codexDesktop.getBackendUpdate().then(() => false, error => error.message.includes('restricted'))",
        ),
        true,
      );
      fs.writeFileSync(
        path.join(app.getPath("userData"), "renderer-fallback.png"),
        (await win.capturePage()).toPNG(),
      );
      assert.equal(
        records()
          .filter((record) => record.event === "renderer-crash")
          .at(-1).failures,
        4,
      );
      assert.equal(
        typeof records().find((record) => record.event === "renderer-crash")
          .exitCode,
        "number",
      );
      // The static fallback can request only the fixed workspace URL.
      await loaded(contents, () => {
        void contents
          .executeJavaScript(
            "document.querySelector('#reload-workspace').click()",
          )
          .catch(() => {});
      });
      assert.equal(contents.getURL(), `${origin}/`);
      // Native Reload targets this known window even while its renderer is dead and unfocused.
      await loaded(contents, () => {
        contents.once("render-process-gone", () => {
          Menu.getApplicationMenu().getMenuItemById("reload-workspace").click();
        });
        contents.forcefullyCrashRenderer();
      });
      assert.equal(contents.getURL(), `${origin}/`);
      await loaded(contents, () => contents.forcefullyCrashRenderer());
      assert.equal(
        records()
          .filter((record) => record.event === "renderer-crash")
          .at(-1).failures,
        1,
      );
      // Exercise the actual production healthy interval with the same live renderer.
      const deadline = Date.now() + 65000;
      while (
        !records().some(
          (record) =>
            record.event === "renderer-healthy" &&
            record.previousFailures === 1,
        )
      ) {
        assert.ok(
          Date.now() < deadline,
          "The healthy renderer did not reset its retry budget",
        );
        await pause(200);
      }
      await loaded(contents, () => contents.forcefullyCrashRenderer());
      assert.equal(
        records()
          .filter((record) => record.event === "renderer-crash")
          .at(-1).failures,
        1,
      );
      const restored = await (await fetch(`${origin}/api/desktop`)).json();
      assert.equal(restored.pid, original.pid);
      assert.equal(win.isVisible(), false);
      assert.equal(
        await contents.executeJavaScript(
          "typeof window.codexDesktop.getBackendUpdate",
        ),
        "function",
      );
      console.log(
        JSON.stringify({
          rendererRecovered: true,
          exhaustedRetries: true,
          nativeReloadWhileCrashed: true,
          fallbackBridgeDenied: true,
          healthyBudgetReset: true,
          backendPid: restored.pid,
          evidence: filename,
        }),
      );
      clearTimeout(timer);
      app.exit(0);
    } catch (error) {
      console.error(error);
      app.exit(1);
    }
  });
});

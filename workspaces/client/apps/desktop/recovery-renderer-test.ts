import { stripTypeScriptTypes } from "node:module";
import type { WebContents } from "electron";
import type { BackendIdentity } from "./backend.ts";
import { present } from "./test-support.ts";
import assert from "node:assert/strict";
import * as fs from "node:fs";
import * as path from "node:path";
import { app, Menu } from "electron";
const rendererSource = stripTypeScriptTypes(
  fs.readFileSync(
    new URL("./fixtures/recovery-renderer-page.ts", import.meta.url),
    "utf8",
  ),
).replaceAll("export ", "");
const evaluate = (
  name: keyof typeof import("./fixtures/recovery-renderer-page.ts"),
) => `${rendererSource}\n${name}()`;
const timer = setTimeout(() => {
  console.error("Renderer recovery timed out.");
  app.exit(1);
}, 110000);
const loaded = (contents: Pick<WebContents, "once">, action: () => void) =>
  new Promise<void>((resolve) => {
    contents.once("did-finish-load", () => resolve());
    action();
  });
import "./main.ts";
app.once("browser-window-created", (_event, win) => {
  const contents = win.webContents;
  contents.once("did-finish-load", async () => {
    try {
      const origin = `http://127.0.0.1:${process.env.CODEX_DESKTOP_PORT}`;
      const original: BackendIdentity = await (
        await fetch(`${origin}/api/desktop`)
      ).json();
      const filename = path.join(
        app.getPath("userData"),
        "renderer-recovery.jsonl",
      );
      // The recovery process writes this journal; parse the fields checked below.
      const records = (): {
        event: string;
        failures: number;
        exitCode: number;
      }[] =>
        fs
          .readFileSync(filename, "utf8")
          .trim()
          .split("\n")
          .map((line) => JSON.parse(line));
      for (let attempt = 0; attempt < 4; attempt++) {
        await loaded(contents, () => contents.forcefullyCrashRenderer());
        assert.equal(win.isVisible(), false);
        assert.equal(contents.getURL() === `${origin}/`, attempt < 3);
      }
      assert.match(contents.getURL(), /^data:text\/html/);
      assert.equal(
        await contents.executeJavaScript(evaluate("heading")),
        "Studio needs to reload",
      );
      assert.equal(
        await contents.executeJavaScript(evaluate("fallbackDenied")),
        true,
      );
      fs.writeFileSync(
        path.join(app.getPath("userData"), "renderer-fallback.png"),
        (await win.capturePage()).toPNG(),
      );
      assert.equal(
        present(
          records()
            .filter((record) => record.event === "renderer-crash")
            .at(-1),
        ).failures,
        4,
      );
      assert.equal(
        typeof present(
          records().find((record) => record.event === "renderer-crash"),
        ).exitCode,
        "number",
      );
      // The static fallback can request only the fixed workspace URL.
      await loaded(contents, () => {
        void contents
          .executeJavaScript(evaluate("reloadWorkspace"))
          .catch(() => {});
      });
      assert.equal(contents.getURL(), `${origin}/`);
      // Native Reload targets this known window even while its renderer is dead and unfocused.
      await loaded(contents, () => {
        contents.once("render-process-gone", () => {
          const menu = present(Menu.getApplicationMenu());
          const item = present(menu.getMenuItemById("reload-workspace"));
          item.click(item, win, {
            shiftKey: false,
            ctrlKey: false,
            altKey: false,
            metaKey: false,
            triggeredByAccelerator: false,
          });
        });
        contents.forcefullyCrashRenderer();
      });
      assert.equal(contents.getURL(), `${origin}/`);
      await loaded(contents, () => contents.forcefullyCrashRenderer());
      assert.equal(
        present(
          records()
            .filter((record) => record.event === "renderer-crash")
            .at(-1),
        ).failures,
        1,
      );
      // Healthy-interval boundaries use fake time in renderer-recovery.test.mjs.
      const restored: BackendIdentity = await (
        await fetch(`${origin}/api/desktop`)
      ).json();
      assert.equal(restored.pid, original.pid);
      assert.equal(win.isVisible(), false);
      assert.equal(
        await contents.executeJavaScript(evaluate("backendUpdateType")),
        "function",
      );
      console.log(
        JSON.stringify({
          rendererRecovered: true,
          exhaustedRetries: true,
          nativeReloadWhileCrashed: true,
          fallbackBridgeDenied: true,
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

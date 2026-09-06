// This entry must run before the regular app's profile, lock, or backend setup.
const { app, BrowserWindow, session, protocol } = require("electron");
const fs = require("node:fs/promises");
const path = require("node:path");

const position = process.argv.indexOf("--render-panel");
const inputPath = process.argv[position + 1];
let window;
let timer;
function finish(error) {
  clearTimeout(timer);
  if (window && !window.isDestroyed()) window.destroy();
  if (error) process.stderr.write(`Panel render failed: ${error.message}\n`);
  app.exit(error ? 1 : 0);
}
if (!inputPath || !path.isAbsolute(inputPath)) {
  throw new Error("An absolute panel render input path is required.");
}
// The caller owns this private temporary directory and removes it after exit.
app.setPath("userData", path.join(path.dirname(inputPath), "profile"));
app.commandLine.appendSwitch("force-device-scale-factor", "1");
app.commandLine.appendSwitch("force-color-profile", "srgb");
app.enableSandbox();
protocol.registerSchemesAsPrivileged([
  {
    scheme: "studio-panel",
    privileges: { standard: true, secure: true, supportFetchAPI: true },
  },
]);
timer = setTimeout(
  () => finish(new Error("The panel renderer timed out.")),
  12000,
);

app
  .whenReady()
  .then(async () => {
    if (process.platform === "darwin") app.setActivationPolicy("prohibited");
    const request = JSON.parse(await fs.readFile(inputPath, "utf8"));
    if (
      !request.panel ||
      typeof request.panel.html !== "string" ||
      typeof request.panel.css !== "string" ||
      !path.isAbsolute(request.preview) ||
      !path.isAbsolute(request.output) ||
      path.dirname(request.output) !== path.dirname(inputPath) ||
      request.width !== 1000 ||
      request.height !== 150
    )
      throw new Error("Invalid panel render request.");
    const dist = path.dirname(request.preview);
    const isolated = session.fromPartition(`panel-render-${process.pid}`, {
      cache: false,
    });
    // A private protocol gives the opaque child frame a normal script origin.
    // No loopback server, workspace route, file URL, or network access is exposed.
    const localFile = (raw) => {
      const url = new URL(raw);
      const relative = decodeURIComponent(url.pathname);
      if (
        url.protocol !== "studio-panel:" ||
        url.host !== "preview" ||
        (!relative.startsWith("/assets/") && relative !== "/panel-preview.html")
      )
        return null;
      const file = path.resolve(dist, "." + relative);
      return file.startsWith(dist + path.sep) ? file : null;
    };
    isolated.protocol.handle("studio-panel", async (request) => {
      const file = localFile(request.url);
      if (!file) return new Response("Not found", { status: 404 });
      const type = {
        ".html": "text/html",
        ".js": "text/javascript",
        ".css": "text/css",
        ".woff2": "font/woff2",
      };
      try {
        return new Response(await fs.readFile(file), {
          headers: {
            "Content-Type":
              type[path.extname(file)] || "application/octet-stream",
          },
        });
      } catch {
        return new Response("Not found", { status: 404 });
      }
    });
    isolated.setPermissionRequestHandler((_contents, _permission, callback) =>
      callback(false),
    );
    isolated.setPermissionCheckHandler(() => false);
    isolated.on("will-download", (event) => event.preventDefault());
    isolated.webRequest.onBeforeRequest((details, callback) => {
      let allowed =
        details.url.startsWith("data:") || details.url === "about:blank";
      try {
        allowed ||= Boolean(localFile(details.url));
      } catch {
        allowed = false;
      }
      callback({ cancel: !allowed });
    });
    window = new BrowserWindow({
      width: request.width,
      height: request.height,
      useContentSize: true,
      show: false,
      focusable: false,
      frame: false,
      skipTaskbar: true,
      backgroundColor: "#18191c",
      webPreferences: {
        session: isolated,
        sandbox: true,
        contextIsolation: true,
        nodeIntegration: false,
        webSecurity: true,
        backgroundThrottling: false,
      },
    });
    window.webContents.setWindowOpenHandler(() => ({ action: "deny" }));
    window.webContents.on("console-message", (event) => {
      if (["warning", "error"].includes(event.level))
        process.stderr.write(`Panel console: ${event.message}\n`);
    });
    window.webContents.on("will-navigate", (event) => event.preventDefault());
    window.webContents.on("will-attach-webview", (event) =>
      event.preventDefault(),
    );
    window.webContents.on("render-process-gone", (_event, details) =>
      finish(new Error(`The panel renderer exited: ${details.reason}.`)),
    );
    await window.loadURL("studio-panel://preview/panel-preview.html");
    // JSON is an argument, not executable agent code. U+2028/U+2029 and '<' are
    // escaped as an extra boundary; the shared document also removes scripts.
    const panel = JSON.stringify(request.panel).replace(
      /[<\u2028\u2029]/g,
      (char) => `\\u${char.charCodeAt(0).toString(16).padStart(4, "0")}`,
    );
    await window.webContents.executeJavaScript(
      `window.renderPanelPreview(${panel})`,
    );
    // Fonts and inline data images have no network dependency. Wait for their
    // decode in the sandboxed frame and pause animations at their initial pose.
    for (const frame of window.webContents.mainFrame.frames) {
      await frame.executeJavaScript(`(async () => {
      await document.fonts.ready;
      await Promise.all(Array.from(document.images, image => image.decode().catch(() => {})));
      document.getAnimations().forEach(animation => { animation.pause(); animation.currentTime = 0; });
    })()`);
    }
    const screenshot = await window.webContents.capturePage(
      { x: 0, y: 0, width: request.width, height: request.height },
      { stayHidden: true, stayAwake: true },
    );
    if (screenshot.isEmpty())
      throw new Error("The panel renderer returned an empty image.");
    // Retina displays can keep a 2x backing scale despite the Chromium switch.
    // Return explicitly labeled CSS-pixel dimensions on every display.
    const size = screenshot.getSize();
    const normalized =
      size.width === request.width && size.height === request.height
        ? screenshot
        : screenshot.resize({
            width: request.width,
            height: request.height,
            quality: "best",
          });
    await fs.writeFile(request.output, normalized.toPNG(), { mode: 0o600 });
    finish();
  })
  .catch(finish);

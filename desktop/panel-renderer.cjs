// This entry must run before the regular app's profile, lock, or backend setup.
const { app, BrowserWindow, session, protocol } = require("electron");
const fs = require("node:fs/promises");
const path = require("node:path");
const measurePanelLayout = require("./panel-layout.cjs");

const position = process.argv.indexOf("--render-panel");
const inputPath = process.argv[position + 1];
let window;
let timer;
function finish(error) {
  clearTimeout(timer);
  if (window && !window.isDestroyed()) window.destroy();
  if (error) process.stderr.write(`Panel render failed: ${error.message || String(error)}\n`);
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
      request.height !== 150 ||
      (request.strictLayout !== undefined &&
        typeof request.strictLayout !== "boolean")
    )
      throw new Error("Invalid panel render request.");
    // A live backend can retain the previous helper while source files update.
    // Its requests were capture-only and omitted this field.
    request.strictLayout ??= false;
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
    // Each responsive layout must fit. Always finish at the screenshot width.
    const viewports = [];
    window.webContents.debugger.attach("1.3");
    const targets =
      await window.webContents.debugger.sendCommand("Target.getTargets");
    const target = targets.targetInfos.find(
      (item) => item.type === "iframe" && item.url === "about:srcdoc",
    );
    const childSession = target
      ? (
          await window.webContents.debugger.sendCommand(
            "Target.attachToTarget",
            { targetId: target.targetId, flatten: true },
          )
        ).sessionId
      : undefined;
    for (const width of [320, 640, 1000]) {
      window.setContentSize(width, request.height);
      await window.webContents.executeJavaScript(
        "new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)))",
      );
      const frames = window.webContents.mainFrame.frames;
      if (frames.length !== 1)
        throw new Error("The panel frame is unavailable.");
      const frame = frames[0];
      await frame.executeJavaScript(`(async () => {
      await document.fonts.ready;
      await Promise.all(Array.from(document.images, image => image.decode().catch(() => {})));
      window.__studioPanelAnimations = document.getAnimations();
      window.__studioPanelAnimations.forEach(animation => { animation.pause(); animation.currentTime = 0; });
    })()`);
      if (request.panel.format === "json-render") {
        await frame.executeJavaScript("window.__studioPanelValidate()");
      }
      const phases = await frame.executeJavaScript(`(() => {
        const animations = window.__studioPanelAnimations;
        if (!animations.length) return [0];
        const values = new Set([0,.25,.5,.75,.999999,1]);
        for (const animation of animations) for (const keyframe of animation.effect.getKeyframes()) values.add(keyframe.computedOffset);
        if (values.size > 32) throw new Error("The panel has too many animation states to validate.");
        return [-1,...Array.from(values).sort((a,b)=>a-b)];
      })()`);
      let aggregate;
      for (const phase of phases) {
        await frame.executeJavaScript(`window.__studioPanelAnimations.forEach(animation => {
          const timing = animation.effect.getComputedTiming();
          animation.pause();
          animation.currentTime = ${phase < 0 ? "0" : `Number(timing.delay || 0) + Number(timing.duration || 0) * ${phase}`};
        })`);
        const measured = await frame.executeJavaScript(
          `(${measurePanelLayout.toString()})()`,
        );
        if (measured.width !== width || measured.height !== request.height)
          throw new Error("The panel viewport has unexpected dimensions.");
        // Pseudo-elements have no DOM getBoundingClientRect. Chromium's layout
        // snapshot includes their real boxes, including negative-position clips.
        const snapshot = await window.webContents.debugger.sendCommand(
          "DOMSnapshot.captureSnapshot",
          {
            computedStyles: [],
            includeDOMRects: true,
          },
          childSession,
        );
        let measuredDocument = false;
        for (const doc of snapshot.documents) {
          if (snapshot.strings[doc.documentURL] !== "about:srcdoc") continue;
          measuredDocument = true;
          const pseudo = doc.nodes.pseudoType;
          for (let index = 0; index < (pseudo?.index.length || 0); index++) {
            const kind = snapshot.strings[pseudo.value[index]];
            if (!["before", "after"].includes(kind)) continue;
            const row = doc.layout.nodeIndex.indexOf(pseudo.index[index]);
            if (row < 0) continue;
            const rootBounds = doc.layout.bounds[0];
            const scaleX = rootBounds[2] / width;
            const scaleY = rootBounds[3] / request.height;
            const raw = doc.layout.bounds[row];
            const [left, top, boxWidth, boxHeight] = [
              raw[0] / scaleX,
              raw[1] / scaleY,
              raw[2] / scaleX,
              raw[3] / scaleY,
            ];
            if (
              left < -0.5 ||
              top < -0.5 ||
              left + boxWidth > width + 0.5 ||
              top + boxHeight > request.height + 0.5
            ) {
              measured.fits = false;
              measured.contentHeight = Math.max(
                measured.contentHeight,
                Math.ceil(
                  Math.max(request.height, top + boxHeight) - Math.min(0, top),
                ),
              );
              measured.contentWidth = Math.max(
                measured.contentWidth,
                Math.ceil(Math.max(width, left + boxWidth) - Math.min(0, left)),
              );
              if (measured.violations.length < 20)
                measured.violations.push({
                  kind: "outside-pseudo",
                  element: `::${kind}`,
                  left,
                  top,
                  right: left + boxWidth,
                  bottom: top + boxHeight,
                });
            }
          }
        }
        if (!measuredDocument)
          throw new Error("The panel layout snapshot is unavailable.");

        if (!aggregate) aggregate = measured;
        else {
          aggregate.fits &&= measured.fits;
          aggregate.contentHeight = Math.max(
            aggregate.contentHeight,
            measured.contentHeight,
          );
          aggregate.contentWidth = Math.max(
            aggregate.contentWidth,
            measured.contentWidth,
          );
          aggregate.violations.push(
            ...measured.violations.slice(
              0,
              Math.max(0, 20 - aggregate.violations.length),
            ),
          );
        }
      }
      await frame.executeJavaScript(
        "window.__studioPanelAnimations.forEach(animation => { animation.pause(); animation.currentTime = 0; })",
      );
      const measured = { ...aggregate, animationSamples: phases.length };
      viewports.push(measured);
    }
    const layout = {
      maxHeight: request.height,
      fits: viewports.every((view) => view.fits),
      viewports,
    };
    await fs.writeFile(request.output + ".json", JSON.stringify(layout), {
      mode: 0o600,
    });
    if (request.strictLayout && !layout.fits) return finish();
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

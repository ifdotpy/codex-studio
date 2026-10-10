const fs = require("node:fs");
const path = require("node:path");

function escapeHTML(value) {
  return String(value).replace(
    /[&<>"']/g,
    (char) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[
        char
      ],
  );
}
function fallbackPage(workspaceURL, crash) {
  return `data:text/html;charset=utf-8,${encodeURIComponent(`<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'; frame-src 'none'"><title>Studio needs to reload</title><style>html{color-scheme:dark;background:#18181b;color:#f4f4f5;font:16px system-ui}body{max-width:540px;margin:15vh auto;padding:32px}h1{font-size:26px;font-weight:600}p{color:#a1a1aa;line-height:1.6}a{display:inline-block;margin-top:12px;border-radius:8px;padding:12px 18px;background:#e4e4e7;color:#18181b;text-decoration:none;font-weight:600}a:focus-visible{outline:3px solid #a78bfa;outline-offset:4px}small{display:block;margin-top:28px;color:#a1a1aa}</style></head><body><h1>Studio needs to reload</h1><p>The workspace closed unexpectedly. Reload to return to your chats.</p><a id="reload-workspace" href="${escapeHTML(workspaceURL)}">Reload workspace</a><small>Reason: ${escapeHTML(crash?.reason || "load failed")}. Exit code: ${escapeHTML(crash?.exitCode ?? "unavailable")}.</small></body></html>`)}`;
}
function createRendererRecovery({
  win,
  workspaceURL,
  profile,
  maxRetries = 3,
  healthyMs = 60000,
  retryDelayMs = 1000,
}) {
  const contents = win.webContents;
  const filename = path.join(profile, "renderer-recovery.jsonl");
  let failures = 0;
  let generation = 0;
  let phase = "loading";
  let fallbackURL;
  let lastCrash;
  let retryTimer;
  let healthyTimer;
  let closing = false;
  const log = (event, extra = {}) => {
    const record = { at: new Date().toISOString(), event, failures, ...extra };
    try {
      fs.mkdirSync(profile, { recursive: true });
      fs.appendFileSync(filename, `${JSON.stringify(record)}\n`, {
        mode: 0o600,
      });
    } catch (error) {
      console.error("Cannot save renderer recovery event:", error.message);
    }
  };
  const clearTimers = () => {
    clearTimeout(retryTimer);
    clearTimeout(healthyTimer);
    retryTimer = healthyTimer = undefined;
  };
  const usable = () =>
    !closing && !win.isDestroyed() && !contents.isDestroyed();
  const showFallback = async () => {
    if (!usable()) return false;
    clearTimers();
    ++generation;
    phase = "fallback";
    fallbackURL = fallbackPage(workspaceURL, lastCrash);
    log("renderer-fallback", lastCrash);
    try {
      await contents.loadURL(fallbackURL);
      return true;
    } catch (error) {
      log("renderer-fallback-failed", { message: error.message });
      return false;
    }
  };
  const reload = async ({ manual = true } = {}) => {
    if (!usable()) return false;
    clearTimers();
    if (manual) failures = 0;
    phase = "loading";
    const attempt = ++generation;
    log(manual ? "renderer-manual-reload" : "renderer-auto-reload");
    try {
      await contents.loadURL(workspaceURL);
      return true;
    } catch (error) {
      if (!usable() || attempt !== generation) return false;
      log("renderer-load-failed", { message: error.message, code: error.code });
      lastCrash = { reason: error.code || "load failed", exitCode: null };
      return showFallback();
    }
  };
  const crashed = (_event, details) => {
    if (!usable()) return;
    clearTimers();
    ++generation;
    lastCrash = { reason: details.reason, exitCode: details.exitCode };
    log("renderer-process-gone", lastCrash);
    const wasFallback = phase === "fallback";
    phase = "crashed";
    ++failures;
    log("renderer-crash", lastCrash);
    if (wasFallback) {
      // If even a static renderer fails, native Reload remains available.
      win.setTitle("Studio needs to reload");
      log("renderer-fallback-crashed", lastCrash);
      return;
    }
    if (failures > maxRetries) {
      void showFallback();
      return;
    }
    retryTimer = setTimeout(() => {
      void reload({ manual: false });
    }, retryDelayMs * failures);
  };
  const loaded = () => {
    if (!usable() || contents.getURL() !== workspaceURL) return;
    phase = "workspace";
    clearTimeout(healthyTimer);
    const rendererPid = contents.getOSProcessId();
    healthyTimer = setTimeout(() => {
      if (
        !usable() ||
        phase !== "workspace" ||
        contents.isCrashed() ||
        contents.getURL() !== workspaceURL ||
        contents.getOSProcessId() !== rendererPid
      )
        return;
      const previousFailures = failures;
      failures = 0;
      log("renderer-healthy", { rendererPid, previousFailures, healthyMs });
    }, healthyMs);
  };
  contents.on("render-process-gone", crashed);
  contents.on("did-finish-load", loaded);
  win.on("close", () => {
    closing = true;
    clearTimers();
  });
  win.on("closed", clearTimers);
  return {
    reload,
    handleNavigation(event) {
      event.preventDefault();
      if (
        phase === "fallback" &&
        event.url === workspaceURL &&
        event.isMainFrame === true &&
        event.frame === contents.mainFrame &&
        (!event.initiator || event.initiator === contents.mainFrame) &&
        contents.getURL() === fallbackURL
      ) {
        void reload();
        return true;
      }
      return false;
    },
  };
}
module.exports = { createRendererRecovery, fallbackPage };

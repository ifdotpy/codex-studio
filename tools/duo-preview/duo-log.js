// Preview-only device logger (not part of Studio). Posts stream and page
// lifecycle events to the localhost preview server for the Duo SSE check.
(() => {
  const t0 = performance.now();
  const queue = [];
  const send = () => {
    if (!queue.length) return;
    const body = JSON.stringify(queue.splice(0));
    // keepalive bodies are limited to 64 KB; stream bodies are larger.
    fetch("./__duo-log", { method: "POST", body, keepalive: body.length < 60000 }).catch(() => {});
  };
  const log = (kind, detail) => {
    queue.push({ t: Math.round(performance.now() - t0), at: new Date().toISOString(), kind, detail });
    if (queue.length > 20) send();
  };
  setInterval(send, 2000);
  log("start", { ua: navigator.userAgent, url: location.href });
  for (const level of ["error", "warn"]) {
    const original = console[level].bind(console);
    console[level] = (...args) => {
      log("console-" + level, args.map((a) => (a instanceof Error ? `${a.name}: ${a.message} ${a.stack?.split("\n").slice(0, 3).join(" | ")}` : typeof a === "string" ? a : (() => { try { return JSON.stringify(a); } catch { return String(a); } })())).join(" ").slice(0, 700));
      original(...args);
    };
  }
  window.addEventListener("error", (e) => log("window-error", `${e.message} ${e.filename}:${e.lineno}`));
  window.addEventListener("unhandledrejection", (e) => log("rejection", String(e.reason?.stack || e.reason).slice(0, 600)));
  log("features", { broadcastChannel: typeof BroadcastChannel, locks: !!navigator.locks, sharedWorker: typeof SharedWorker, indexedDB: !!window.indexedDB });
  for (const type of ["visibilitychange", "pagehide", "pageshow", "online", "offline", "focus", "blur"])
    window.addEventListener(type, () => log(type, { state: document.visibilityState, online: navigator.onLine }));
  document.addEventListener("visibilitychange", () => log("visibility", document.visibilityState));
  const NativeES = window.EventSource;
  window.EventSource = function (url, init) {
    const es = new NativeES(url, init);
    const short = String(url).split("?")[0];
    log("es-new", short);
    es.addEventListener("open", () => log("es-open", short));
    es.addEventListener("error", () => log("es-error", { url: short, readyState: es.readyState }));
    return es;
  };
  window.EventSource.prototype = NativeES.prototype;
  Object.assign(window.EventSource, { CONNECTING: 0, OPEN: 1, CLOSED: 2 });
  const nativeFetch = window.fetch.bind(window);
  window.fetch = async (input, init) => {
    const url = typeof input === "string" ? input : input?.url || String(input);
    const stream = url.includes("/api/sync/stream");
    const short = url.split("?")[0];
    try {
      const response = await nativeFetch(input, init);
      if (stream || !response.ok) {
        const entry = { url: short, method: init?.method || input?.method || "GET", status: response.status, type: response.headers.get("content-type") };
        if (stream) {
          entry.fullUrl = url;
          try {
            const q = new URL(url, location.href).searchParams;
            entry.resources = JSON.parse(q.get("resources") || "[]").map((r) => r.kind + (r.agentId ? ":" + r.agentId.slice(0, 6) : "")).join(",");
            entry.reconnect = q.get("reconnect");
          } catch {}
        }
        if (!response.ok) {
          try { entry.body = (await response.clone().text()).slice(0, 400); } catch {}
          try { entry.reqBody = typeof init?.body === "string" ? init.body.slice(0, 300) : undefined; } catch {}
        }
        log("fetch", entry);
      }
      if (stream && response.body) {
        const [a, b] = response.body.tee();
        const reader = b.getReader();
        let bytes = 0, chunks = 0, last = performance.now();
        const decoder = new TextDecoder();
        let text = "";
        (async () => {
          try {
            for (;;) {
              const { done, value } = await reader.read();
              if (done) { log("stream-end", { url: short, bytes, chunks }); log("stream-body", { fullUrl: url, text }); break; }
              bytes += value.length; chunks++;
              if (text.length < 61440) text += decoder.decode(value, { stream: true }).slice(0, 61440 - text.length);
              const now = performance.now();
              if (now - last > 20000) log("stream-gap", { ms: Math.round(now - last), chunks });
              last = now;
              if (chunks <= 3 || chunks % 50 === 0) log("stream-chunk", { chunks, bytes });
            }
          } catch (error) { log("stream-error", { url: short, error: String(error), bytes, chunks }); log("stream-body", { fullUrl: url, text }); }
        })();
        return new Response(a, { status: response.status, statusText: response.statusText, headers: response.headers });
      }
      return response;
    } catch (error) {
      if (stream) log("fetch-error", { url: short, error: String(error) });
      throw error;
    }
  };
  let hingeKey = "";
  let regionsKey = "";
  window.addEventListener("studio-native-viewport", (event) => {
    const hinge = event.detail?.hinge;
    const key = hinge ? `${hinge.status}:${hinge.angle}` : "";
    if (hinge && key !== hingeKey) {
      hingeKey = key;
      log("hinge", { ...hinge, attr: document.documentElement.getAttribute("data-native-hinge"), css: document.documentElement.style.getPropertyValue("--native-hinge-angle"), inner: [innerWidth, innerHeight] });
    }
    const column = event.detail?.statusColumn;
    const band = event.detail?.hingeBand;
    const regionKey = JSON.stringify([column, band, innerWidth, innerHeight]);
    if (regionKey !== regionsKey) {
      regionsKey = regionKey;
      // Measure the rail after React applied the new geometry.
      setTimeout(() => {
        const rail = document.querySelector(".edge-rail");
        const b = rail && rail.getBoundingClientRect();
        log("regions", { statusColumn: column, hingeBand: band, inner: [innerWidth, innerHeight], rail: b && [b.x, b.y, b.width, b.height].map(Math.round), railButtons: rail ? rail.querySelectorAll(".edge-rail-button").length : 0 });
      }, 600);
    }
  });
  let banner = "";
  setInterval(() => {
    const text = document.body?.innerText || "";
    const now = ["Live updates are reconnecting", "Connection lost", "Offline."].filter((s) => text.includes(s)).join("|");
    if (now !== banner) {
      banner = now;
      const hit = [...document.querySelectorAll("body *")].filter((n) => n.children.length < 40 && /Connection lost/.test(n.textContent || "")).pop();
      let node = hit;
      for (let i = 0; i < 4 && node?.parentElement; i++) node = node.parentElement;
      const card = (node?.textContent || "").replace(/\s+/g, " ").slice(0, 600);
      log("banner", { now: now || "none", card });
    }
  }, 1000);
})();

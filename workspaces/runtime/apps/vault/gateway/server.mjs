// The vault gateway's HTTP side (docs/vault.md, "Gateway"): the site, and the API of every vault
// in the configuration. main.mjs serves it with TLS on the tailnet address only.
import { createReadStream, statSync } from "node:fs";
import { extname, join, normalize } from "node:path";
import { pipeline } from "node:stream/promises";
import { VaultError } from "../src/service.mjs";

const SECURITY = {
  "content-security-policy":
    "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; " +
    "connect-src 'self'; img-src 'self' blob: data:; media-src 'self' blob:; frame-src blob:; " +
    "object-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'",
  "strict-transport-security": "max-age=63072000",
  "x-content-type-options": "nosniff",
  "x-frame-options": "DENY",
  "referrer-policy": "no-referrer",
  "cross-origin-opener-policy": "same-origin",
};
const TYPES = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript",
  ".css": "text/css",
  ".wasm": "application/wasm",
  ".json": "application/json",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".ico": "image/x-icon",
};
const PROJECT = /^[A-Za-z0-9][A-Za-z0-9_-]{0,43}$/;
const BODY_LIMIT = 16 * 1024;

/**
 * `config()` gives {host, projects: {name: ...}}; `vaultFor(name)` gives a service.mjs vault or
 * null; `site` is the folder of the built site.
 */
export function makeGateway({ config, vaultFor, site }) {
  const root = normalize(`${site}/`);
  function send(res, status, body, headers = {}) {
    res.writeHead(status, {
      ...SECURITY,
      "content-type": "application/json",
      "cache-control": "no-store",
      ...headers,
    });
    res.end(typeof body === "string" ? body : JSON.stringify(body));
  }

  async function readJson(req) {
    let size = 0;
    const chunks = [];
    for await (const chunk of req) {
      size += chunk.length;
      if (size > BODY_LIMIT) throw new VaultError(413, "the body is too large");
      chunks.push(chunk);
    }
    try {
      return JSON.parse(Buffer.concat(chunks).toString() || "{}");
    } catch {
      throw new VaultError(400, "the body is not JSON");
    }
  }

  function serveFile(req, res, path) {
    const file = normalize(join(root, path));
    let stat;
    try {
      stat = file.startsWith(root) && statSync(file);
    } catch {
      stat = null;
    }
    if (!stat?.isFile()) return send(res, 404, { error: "no such file" });
    res.writeHead(200, {
      ...SECURITY,
      "content-type": TYPES[extname(file)] ?? "application/octet-stream",
      "content-length": stat.size,
      // Vite names the files below assets/ by their content.
      "cache-control": path.startsWith("/assets/")
        ? "public, max-age=31536000, immutable"
        : "no-cache",
    });
    if (req.method === "HEAD") return res.end();
    createReadStream(file).pipe(res);
  }

  async function api(req, res, parts) {
    const { projects = {} } = config();
    if (parts.length === 1 && parts[0] === "projects" && req.method === "GET")
      return send(res, 200, { projects: Object.keys(projects).sort() });
    if (parts[0] !== "p" || !PROJECT.test(parts[1] ?? ""))
      return send(res, 404, { error: "no such route" });
    const vault = vaultFor(parts[1]);
    if (!vault) return send(res, 404, { error: "no such vault" });
    const [, , kind, name, extra] = parts;
    if (extra !== undefined) return send(res, 404, { error: "no such route" });
    const route = `${req.method} ${kind}${name === undefined ? "" : "/*"}`;

    if (route === "GET slots")
      return send(res, 200, { slots: await vault.slots() });
    if (route === "GET challenge")
      return send(res, 200, { challenge: await vault.challenge() });

    const auth = String(req.headers.authorization ?? "");
    if (!auth.startsWith("Bearer ") || !vault.valid(auth.slice(7)))
      return send(res, 401, {
        error: "the token is missing, wrong or expired",
      });

    if (route === "GET states")
      return send(res, 200, { states: await vault.states() });
    if (route === "GET states/*") {
      const { body, length } = await vault.object(name);
      res.writeHead(200, {
        ...SECURITY,
        "content-type": "application/octet-stream",
        "cache-control": "private, no-store",
        ...(length === undefined ? {} : { "content-length": length }),
      });
      await pipeline(body, res);
      return;
    }
    if (route === "PUT slots/*") {
      const { data } = await readJson(req);
      await vault.addSlot(name, data);
      return send(res, 200, { added: name });
    }
    if (route === "DELETE slots/*") {
      await vault.removeSlot(name);
      return send(res, 200, { removed: name });
    }
    return send(res, 404, { error: "no such route" });
  }

  return async function handle(req, res) {
    try {
      // A name that resolves to the gateway is not enough: the host must be the configured one.
      const { host } = config();
      const given = String(req.headers.host ?? "").replace(/:\d+$/, "");
      if (host && given !== host)
        return send(res, 421, { error: "wrong host name" });
      const url = new URL(req.url, "http://gateway");
      let parts;
      try {
        parts = url.pathname
          .split("/")
          .filter(Boolean)
          .map((p) => decodeURIComponent(p));
      } catch {
        return send(res, 400, { error: "the path is not valid" });
      }
      if (parts[0] === "api") return await api(req, res, parts.slice(1));
      if (req.method !== "GET" && req.method !== "HEAD")
        return send(res, 405, { error: "the site is read-only" });
      // Each vault is a page of the site: /<project>/ shows the same index.html.
      if (!extname(url.pathname)) return serveFile(req, res, "/index.html");
      return serveFile(req, res, url.pathname);
    } catch (error) {
      if (error instanceof VaultError)
        return res.headersSent
          ? res.destroy()
          : send(res, error.status, { error: error.message });
      console.error(error);
      return res.headersSent
        ? res.destroy()
        : send(res, 500, { error: "internal error" });
    }
  };
}

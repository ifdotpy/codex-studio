const http = require("node:http");
const fs = require("node:fs/promises");
const path = require("node:path");
const contentTypes = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".json": "application/json",
  ".webmanifest": "application/manifest+json",
  ".png": "image/png",
  ".svg": "image/svg+xml",
  ".ico": "image/x-icon",
  ".woff2": "font/woff2",
};
async function startUiHost({ resources, port = 4621 }) {
  if (!Number.isInteger(port) || port < 0 || port > 65535)
    throw new Error("Invalid UI port.");
  const root = await fs.realpath(path.join(resources, "web/dist"));
  await fs.access(path.join(root, "index.html"));
  const server = http.createServer(async (request, response) => {
    response.setHeader("X-Content-Type-Options", "nosniff");
    response.setHeader("Referrer-Policy", "same-origin");
    const fail = (status) => {
      response.writeHead(status);
      response.end();
    };
    if (!["GET", "HEAD"].includes(request.method)) return fail(405);
    // The UI host owns no API or state. It never forwards a local API request.
    try {
      const url = new URL(request.url, "http://127.0.0.1");
      const pathname = decodeURIComponent(url.pathname);
      if (
        pathname.startsWith("/api/") ||
        pathname.includes("\0") ||
        pathname.includes("\\")
      )
        return fail(404);
      const name = pathname === "/" ? "index.html" : pathname.slice(1);
      const target = await fs.realpath(path.join(root, name));
      if (!target.startsWith(root + path.sep)) return fail(404);
      if (!(await fs.stat(target)).isFile()) return fail(404);
      const bytes = await fs.readFile(target);
      response.setHeader(
        "Content-Security-Policy",
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self' https://api.openai.com https://*.ts.net; img-src 'self' data: blob: https: http:; media-src 'self' blob: data:; frame-src 'self' blob: http://*.localhost:*; frame-ancestors " +
          (name === "index.html" &&
          /^[a-zA-Z0-9_-]{1,128}$/.test(
            url.searchParams.get("studio-server") || "",
          )
            ? "'self' http://127.0.0.1:* http://localhost:*"
            : "'none'") +
          "; base-uri 'none'",
      );
      response.setHeader(
        "Content-Type",
        contentTypes[path.extname(target)] || "application/octet-stream",
      );
      response.setHeader("Content-Length", bytes.length);
      response.setHeader(
        "Cache-Control",
        name.startsWith("assets/")
          ? "public, max-age=31536000, immutable"
          : "no-cache",
      );
      response.writeHead(200);
      response.end(request.method === "HEAD" ? undefined : bytes);
    } catch {
      fail(404);
    }
  });
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(port, "127.0.0.1", resolve);
  });
  return {
    origin: `http://127.0.0.1:${server.address().port}`,
    pid: process.pid,
    owned: false,
    uiOnly: true,
    close: () =>
      new Promise((resolve, reject) =>
        server.close((error) => (error ? reject(error) : resolve())),
      ),
  };
}
module.exports = { startUiHost };

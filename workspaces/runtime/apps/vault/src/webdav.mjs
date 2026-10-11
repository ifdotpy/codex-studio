// A read-only WebDAV view of the latest vault state for Finder (docs/vault.md, "Mac view").
// Files decrypt when they are read and stay in a cache folder, keyed by their SHA-256.
import { createHash } from "node:crypto";
import {
  existsSync,
  mkdirSync,
  readFileSync,
  renameSync,
  writeFileSync,
} from "node:fs";
import http from "node:http";
import { join, posix } from "node:path";

const xml = (text) =>
  String(text).replace(/[<>&"']/g, (c) => `&#${c.charCodeAt(0)};`);
const href = (path, folder) =>
  "/" +
  path.split("/").filter(Boolean).map(encodeURIComponent).join("/") +
  (folder && path ? "/" : "");

/** An index of one state: folders with their children, and files. */
export function index(files) {
  const folders = new Map([["", new Set()]]);
  for (const path of Object.keys(files)) {
    const parts = path.split("/");
    for (let i = 1; i < parts.length; i++) {
      const parent = parts.slice(0, i - 1).join("/");
      const folder = parts.slice(0, i).join("/");
      if (!folders.has(folder)) folders.set(folder, new Set());
      folders.get(parent).add(folder);
    }
    folders.get(parts.slice(0, -1).join("/")).add(path);
  }
  return folders;
}

/**
 * @param {{load: () => Promise<{files: object, time: number}>, read: (entry: any) => Promise<Uint8Array>, cache: string, refreshMs?: number}} options
 */
export function view({ load, read, cache, refreshMs = 5 * 60 * 1000 }) {
  mkdirSync(cache, { recursive: true, mode: 0o700 });
  let snapshot;
  let loading;
  const refresh = async () => {
    const { files, time } = await load();
    const next = {
      files,
      time: new Date(time * 1000),
      folders: index(files),
      links: new Map(),
    };
    // A link shows the file it points to inside the state; resolve links now (they are few), so
    // a listing gives the size of what a read returns.
    for (const [path, entry] of Object.entries(files)) {
      let target = path;
      let hop = entry;
      for (let i = 0; i < 8 && hop?.[0] === "120000"; i++) {
        const text = (await raw(target, hop)).toString();
        target = posix.normalize(posix.join(posix.dirname(target), text));
        hop = files[target];
      }
      if (entry[0] === "120000")
        next.links.set(path, hop && hop[0] !== "120000" ? hop : entry);
    }
    snapshot = next;
  };
  const current = async () => {
    if (!snapshot || Date.now() - snapshot.loaded > refreshMs) {
      loading ??= refresh()
        .then(() => (snapshot.loaded = Date.now()))
        .finally(() => (loading = null));
      if (!snapshot) await loading;
      else loading.catch(() => {});
    }
    return snapshot;
  };

  /** The stored bytes of one entry, checked against the tree and kept in the cache. */
  async function raw(path, entry) {
    const file = join(cache, entry[2]);
    if (existsSync(file)) return readFileSync(file);
    const data = Buffer.from(await read(entry));
    if (createHash("sha256").update(data).digest("hex") !== entry[2])
      throw new Error(`${path}: content differs from the tree`);
    writeFileSync(file + ".part", data, { mode: 0o600 });
    renameSync(file + ".part", file);
    return data;
  }

  /** A file's content; a link gives the content of the file it points to. */
  async function content(path, entry, state) {
    return raw(path, state.links.get(path) ?? entry);
  }

  function properties(path, state) {
    const folder = state.folders.has(path);
    const entry = state.links.get(path) ?? state.files[path];
    const name = path.split("/").pop() || "/";
    return (
      `<D:response><D:href>${xml(href(path, folder))}</D:href><D:propstat><D:prop>` +
      `<D:displayname>${xml(name)}</D:displayname>` +
      (folder
        ? "<D:resourcetype><D:collection/></D:resourcetype>"
        : `<D:resourcetype/><D:getcontentlength>${entry[1]}</D:getcontentlength><D:getcontenttype>application/octet-stream</D:getcontenttype><D:getetag>"${entry[2]}"</D:getetag>`) +
      `<D:getlastmodified>${state.time.toUTCString()}</D:getlastmodified><D:creationdate>${state.time.toISOString()}</D:creationdate>` +
      `</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>`
    );
  }

  return http.createServer(async (request, response) => {
    try {
      const path = decodeURIComponent(
        new URL(request.url, "http://localhost").pathname,
      ).replace(/^\/+|\/+$/g, "");
      const state = await current();
      const exists = state.folders.has(path) || path in state.files;
      if (request.method === "OPTIONS") {
        response
          .writeHead(200, {
            DAV: "1",
            Allow: "OPTIONS, GET, HEAD, PROPFIND",
            "MS-Author-Via": "DAV",
          })
          .end();
      } else if (request.method === "PROPFIND") {
        if (!exists) return response.writeHead(404).end();
        const children =
          request.headers.depth !== "0" && state.folders.has(path)
            ? [...state.folders.get(path)].sort()
            : [];
        const body = `<?xml version="1.0" encoding="utf-8"?><D:multistatus xmlns:D="DAV:">${[path, ...children].map((p) => properties(p, state)).join("")}</D:multistatus>`;
        response
          .writeHead(207, {
            "content-type": "application/xml; charset=utf-8",
            "content-length": Buffer.byteLength(body),
          })
          .end(body);
      } else if (request.method === "GET" || request.method === "HEAD") {
        const entry = state.files[path];
        if (!entry)
          return response.writeHead(state.folders.has(path) ? 405 : 404).end();
        const data = await content(path, entry, state);
        const range = /^bytes=(\d*)-(\d*)$/.exec(request.headers.range ?? "");
        let start = 0;
        let end = data.length - 1;
        if (range && (range[1] || range[2])) {
          start = range[1]
            ? Number(range[1])
            : Math.max(0, data.length - Number(range[2]));
          end = range[1] && range[2] ? Math.min(Number(range[2]), end) : end;
          if (start > end)
            return response
              .writeHead(416, { "content-range": `bytes */${data.length}` })
              .end();
        }
        const headers = {
          "content-length": end - start + 1,
          "accept-ranges": "bytes",
          etag: `"${entry[2]}"`,
          "last-modified": state.time.toUTCString(),
          "content-type": "application/octet-stream",
        };
        if (range && (range[1] || range[2]))
          headers["content-range"] = `bytes ${start}-${end}/${data.length}`;
        response.writeHead(
          range && (range[1] || range[2]) ? 206 : 200,
          headers,
        );
        response.end(
          request.method === "HEAD" ? undefined : data.subarray(start, end + 1),
        );
      } else {
        // Read only: Finder mounts it without write access.
        response
          .writeHead(405, { Allow: "OPTIONS, GET, HEAD, PROPFIND" })
          .end();
      }
    } catch (error) {
      if (!response.headersSent)
        response.writeHead(500, { "content-type": "text/plain" });
      response.end(String(error.message));
    }
  });
}

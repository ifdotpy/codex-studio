// Vite prefixes this file with the exact build and static asset lists.
if (self.STUDIO_SHELL) {
  const { build, initial, allowed } = self.STUDIO_SHELL;
  const prefix = "studio-shell-v1-";
  const cacheName = prefix + build;
  const paths = new Set(allowed);
  const manifestPath = "/__studio_shell_manifest__";
  const hashedAsset = /^\/assets\/[^/]+-[\w-]{8,}\.[a-z0-9]+$/;
  const staticPath = (path) =>
    typeof path === "string" &&
    (["/", "/manifest.webmanifest", "/apple-touch-icon.png"].includes(path) ||
      hashedAsset.test(path));
  const shellCaches = async () => {
    const names = (await caches.keys())
      .filter((name) => name.startsWith(prefix))
      .reverse();
    const complete = [];
    for (const name of names) {
      const cache = await caches.open(name);
      const manifest = await (await cache.match(manifestPath))
        ?.json()
        .catch(() => null);
      if (
        !manifest ||
        prefix + manifest.build !== name ||
        !Array.isArray(manifest.allowed) ||
        !manifest.allowed.every(staticPath) ||
        !Array.isArray(manifest.initial) ||
        !manifest.initial.includes("/") ||
        !manifest.initial.every((path) => manifest.allowed.includes(path))
      )
        continue;
      const html = await cache.match("/");
      if (
        !html ||
        !(await html.clone().text()).includes(
          `name="studio-build" content="${manifest.build}"`,
        )
      )
        continue;
      complete.push({ cache, manifest, html });
    }
    return complete;
  };
  const cachedShell = async () => {
    for (const entry of await shellCaches()) {
      // The index is the install commit marker. Check the closure as well so
      // partial storage loss cannot select a shell whose modules are absent.
      if (
        (
          await Promise.all(
            entry.manifest.initial.map((path) => entry.cache.match(path)),
          )
        ).every(Boolean)
      )
        return entry.html;
    }
  };
  const isCurrentHTML = (text) =>
    text.includes(`name="studio-build" content="${build}"`);
  const valid = (response) =>
    response.ok && response.type !== "opaque" && !response.redirected;
  const currentHTML = async (response) =>
    valid(response) &&
    (response.headers.get("content-type") || "").includes("text/html") &&
    isCurrentHTML(await response.clone().text());

  self.addEventListener("install", (event) => {
    event.waitUntil(
      (async () => {
        const cache = await caches.open(cacheName);
        try {
          // A deployment during installation must not produce a partial shell.
          const shell = await fetch("/", {
            cache: "reload",
            redirect: "error",
          });
          if (!(await currentHTML(shell)))
            throw new Error("Studio build changed during installation.");
          await Promise.all(
            initial
              .filter((path) => path !== "/")
              .map(async (path) => {
                const response = await fetch(path, {
                  cache: path.startsWith("/assets/") ? "force-cache" : "reload",
                  redirect: "error",
                });
                if (!valid(response))
                  throw new Error(`Studio asset unavailable: ${path}`);
                await cache.put(path, response);
              }),
          );
          await cache.put(
            manifestPath,
            new Response(JSON.stringify({ build, initial, allowed }), {
              headers: { "Content-Type": "application/json" },
            }),
          );
          await cache.put("/", shell);
        } catch (error) {
          await caches.delete(cacheName);
          throw error;
        }
      })(),
    );
  });

  self.addEventListener("activate", (event) => {
    event.waitUntil(
      (async () => {
        const names = (await caches.keys()).filter((name) =>
          name.startsWith(prefix),
        );
        const previous = names.filter((name) => name !== cacheName).slice(-1);
        await Promise.all(
          names
            .filter((name) => name !== cacheName && !previous.includes(name))
            .map((name) => caches.delete(name)),
        );
      })(),
    );
  });

  self.addEventListener("fetch", (event) => {
    const request = event.request;
    const url = new URL(request.url);
    if (request.method !== "GET" || url.origin !== self.location.origin) return;
    if (
      request.mode === "navigate" &&
      (url.pathname === "/" || url.pathname === "/index.html")
    ) {
      event.respondWith(
        (async () => {
          const cache = await caches.open(cacheName).catch(() => null);
          const cached = await cachedShell().catch(() => undefined);
          let timer;
          const network = fetch(request).then(async (response) => {
            if (!valid(response)) throw new Error("Studio page unavailable.");
            if (await currentHTML(response))
              await cache?.put("/", response.clone()).catch(() => {});
            else void self.registration.update().catch(() => {});
            return response;
          });
          // Keep a late same-build response, but do not hold a cached page behind
          // a laptop that sleeps or an unreliable mobile connection.
          event.waitUntil(network.catch(() => {}));
          if (!cached) return network;
          try {
            return await Promise.race([
              network,
              new Promise((resolve) => {
                timer = setTimeout(() => resolve(cached), 1200);
              }),
            ]);
          } catch {
            return cached;
          } finally {
            clearTimeout(timer);
          }
        })(),
      );
      return;
    }
    if (
      url.search ||
      url.pathname === "/" ||
      (!paths.has(url.pathname) && !hashedAsset.test(url.pathname))
    )
      return;
    event.respondWith(
      (async () => {
        const cache = await caches.open(cacheName).catch(() => null);
        const cached = await cache?.match(url.pathname).catch(() => undefined);
        if (cached) return cached;
        // A waiting worker can already have a complete newer build. Its cached
        // HTML must retain its own exact assets without claiming a live page.
        for (const entry of await shellCaches().catch(() => [])) {
          if (!entry.manifest.allowed.includes(url.pathname)) continue;
          const newer = await entry.cache.match(url.pathname);
          if (newer) return newer;
        }
        const response = await fetch(request);
        if (paths.has(url.pathname) && valid(response))
          await cache?.put(url.pathname, response.clone()).catch(() => {});
        return response;
      })(),
    );
  });
}

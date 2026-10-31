/**
 * Ordered routes of one paired server. The paired origin is first; origins the
 * server reports are learned and follow it. A learned route gets requests only
 * after it signs a fresh nonce with the server key stored at pairing. The HTTPS
 * paired origin is bound to its host name by TLS and needs no proof. Identity
 * and return checks are connection checks. They never read server data
 * (docs/sync-protocol.md).
 */

import { newIdentityNonce, verifyIdentityProof } from "./pairing";

/** Longest wait for one identity answer, in milliseconds. */
export const ROUTE_IDENTITY_TIMEOUT_MS = 5_000;
/** Wait before a failed route is tried again, in milliseconds. */
export const ROUTE_FAILURE_COOLDOWN_MS = 30_000;
/** Period of the check for a faster route while a later route is in use, in milliseconds. */
export const ROUTE_RETURN_INTERVAL_MS = 60_000;

/** The public identity of a server; it needs no key. */
const IDENTITY_PATH = "/api/multi-server/v1/identity";
const STORAGE_PREFIX = "studio-routes-v1:";

type RouteStore = {
  read(): string[];
  write(learned: string[]): void;
};

/** A route answered with the identity of another server. */
export class RouteIdentityError extends Error {
  constructor() {
    super("No route answers with the identity of this server.");
  }
}

/** The origin of a reported route, or undefined when it cannot be one. */
export function routeOrigin(value: unknown): string | undefined {
  if (typeof value !== "string") return;
  try {
    const url = new URL(value);
    if (
      (url.protocol !== "https:" && url.protocol !== "http:") ||
      url.username ||
      url.password
    )
      return;
    return url.origin;
  } catch {
    return;
  }
}

export class ServerRoutes {
  private learned: string[];
  private active: string | undefined;
  private pending: Promise<string> | undefined;
  private readonly cooldowns = new Map<string, number>();
  private readonly listeners = new Set<(origin: string) => void>();
  readonly paired: string;
  private readonly serverId: string;
  private readonly store: RouteStore;
  private readonly request: typeof fetch;
  private publicKey: string | undefined;

  constructor(
    paired: string,
    serverId: string,
    store: RouteStore,
    request: typeof fetch = (...args) => fetch(...args),
  ) {
    this.paired = paired;
    this.serverId = serverId;
    this.store = store;
    this.request = request;
    this.learned = store.read();
  }

  /** The routes in order of preference. A key paired over HTTPS never goes over HTTP. */
  routes(): string[] {
    const secure = new URL(this.paired).protocol === "https:";
    return [
      this.paired,
      ...this.learned.filter(
        (origin) =>
          origin !== this.paired && (!secure || origin.startsWith("https:")),
      ),
    ];
  }

  /** The server key (PEM) stored at pairing. Without it no learned route can prove itself. */
  pin(publicKey: string) {
    this.publicKey = publicKey;
  }

  current(): string | undefined {
    return this.active;
  }

  /** Called when the route in use changes; the open stream must be replaced. */
  onChange(listener: (origin: string) => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  /** The origins an authenticated server reports now. Missing ones are dropped. */
  learn(reported: readonly unknown[]) {
    const next = [
      ...new Set(
        reported.flatMap((value) => {
          const origin = routeOrigin(value);
          return origin && origin !== this.paired ? [origin] : [];
        }),
      ),
    ];
    if (next.join("\n") === this.learned.join("\n")) return;
    this.learned = next;
    for (const origin of this.cooldowns.keys())
      if (!this.routes().includes(origin)) this.cooldowns.delete(origin);
    this.store.write(next);
  }

  /** A request on this route failed at the network level. */
  fail(origin: string, now = Date.now()) {
    this.cooldowns.set(origin, now + ROUTE_FAILURE_COOLDOWN_MS);
    if (this.active === origin) this.active = undefined;
  }

  /** The verified route to use. Checks routes in order until one answers. */
  resolve(): Promise<string> {
    if (this.active) return Promise.resolve(this.active);
    return (this.pending ??= this.select().finally(() => {
      this.pending = undefined;
    }));
  }

  /**
   * While a later route is in use, check the earlier ones and move back to the
   * first that answers. Called on network change, foreground and every minute.
   */
  async returnToFaster(): Promise<void> {
    if (!this.active) return;
    const routes = this.routes();
    const index = routes.indexOf(this.active);
    for (const origin of routes.slice(0, Math.max(index, 0))) {
      if (!this.ready(origin)) continue;
      if ((await this.check(origin)) === "ok") {
        this.setActive(origin);
        return;
      }
    }
  }

  private ready(origin: string) {
    return (this.cooldowns.get(origin) ?? 0) <= Date.now();
  }

  private async select(): Promise<string> {
    let mismatch = false;
    // When every route waits, the retry loop still needs an answer: try them all.
    const all = this.routes();
    if (all.length === 1) {
      this.setActive(all[0]!);
      return all[0]!;
    }
    const open = all.filter((origin) => this.ready(origin));
    for (const origin of open.length ? open : all) {
      const result = await this.check(origin);
      if (result === "ok") {
        this.setActive(origin);
        return origin;
      }
      mismatch ||= result === "mismatch";
    }
    throw mismatch
      ? new RouteIdentityError()
      : new TypeError("No server route answers.");
  }

  private setActive(origin: string) {
    const changed = this.active !== undefined && this.active !== origin;
    this.active = origin;
    if (changed) for (const listener of this.listeners) listener(origin);
  }

  /** Asks a route to prove the server identity before any key or session reaches it. */
  private async check(origin: string): Promise<"ok" | "mismatch" | "down"> {
    const nonce = newIdentityNonce();
    try {
      const response = await this.request(
        `${origin}${IDENTITY_PATH}?nonce=${nonce}`,
        {
          cache: "no-store",
          credentials: "omit",
          redirect: "error",
          signal: AbortSignal.timeout(ROUTE_IDENTITY_TIMEOUT_MS),
        },
      );
      const body: unknown = response.ok ? await response.json() : undefined;
      const field = (name: string) =>
        body && typeof body === "object" && name in body
          ? (body as Record<string, unknown>)[name]
          : undefined;
      if (typeof field("serverId") !== "string")
        throw new TypeError("No identity.");
      const proved =
        origin === this.paired && origin.startsWith("https:")
          ? true
          : typeof field("proof") === "string" &&
            this.publicKey !== undefined &&
            (await verifyIdentityProof(
              this.publicKey,
              this.serverId,
              nonce,
              field("proof") as string,
            ));
      if (field("serverId") === this.serverId && proved) return "ok";
      this.fail(origin);
      return "mismatch";
    } catch {
      this.fail(origin);
      return "down";
    }
  }
}

/** The request for `request.url`'s path on another origin of the same server. */
export async function retarget(
  request: Request,
  origin: string,
): Promise<Request> {
  const url = new URL(request.url);
  if (url.origin === origin) return request;
  const hasBody = request.method !== "GET" && request.method !== "HEAD";
  return new Request(new URL(url.pathname + url.search, origin), {
    method: request.method,
    headers: request.headers,
    ...(hasBody ? { body: await request.arrayBuffer() } : {}),
    signal: request.signal,
    cache: request.cache,
    credentials: request.credentials,
    redirect: request.redirect,
  });
}

/** Sends a request on a verified route. A network failure puts the route in cooldown. */
export async function sendRouted(
  routes: ServerRoutes,
  request: Request,
  send: typeof fetch = (...args) => fetch(...args),
): Promise<Response> {
  try {
    return await send(request);
  } catch (error) {
    if (error instanceof TypeError) routes.fail(new URL(request.url).origin);
    throw error;
  }
}

const registry = new Map<string, ServerRoutes>();
let listening = false;

function localStore(serverId: string): RouteStore {
  const key = STORAGE_PREFIX + serverId;
  return {
    read() {
      try {
        const saved: unknown = JSON.parse(localStorage.getItem(key) ?? "[]");
        return Array.isArray(saved)
          ? saved.flatMap((item) => routeOrigin(item) ?? [])
          : [];
      } catch {
        return [];
      }
    },
    write(learned) {
      try {
        localStorage.setItem(key, JSON.stringify(learned));
      } catch {
        // Routes still work for this page without storage.
      }
    },
  };
}

/** Checks earlier routes of every server. */
function wakeAll() {
  for (const routes of registry.values()) void routes.returnToFaster();
}

/** One owner of the routes of a server in this page. */
export function routesFor(server: {
  id: string;
  origin: string;
}): ServerRoutes {
  let routes = registry.get(server.id);
  if (!routes || routes.paired !== server.origin) {
    routes = new ServerRoutes(server.origin, server.id, localStore(server.id));
    registry.set(server.id, routes);
  }
  if (
    !listening &&
    typeof window !== "undefined" &&
    typeof document !== "undefined"
  ) {
    listening = true;
    window.addEventListener("online", wakeAll);
    document.addEventListener("visibilitychange", () => {
      if (!document.hidden) wakeAll();
    });
    setInterval(wakeAll, ROUTE_RETURN_INTERVAL_MS);
  }
  return routes;
}

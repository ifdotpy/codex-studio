import {
  createNativeClient,
  invokeClientRequest,
  type NativeInvoke,
} from "./clientCore";
export type * from "./clientCore";

/** Rust ui_events.rs owns these fixed broker contracts; the focused test checks parity. */
export const HOST_EVENT_CONNECT_METHOD = "ui.events.connect";
export const HOST_EVENT_DISCONNECT_METHOD = "ui.events.disconnect";
export const HOST_EVENT_TOPICS = [
  "studio-client-event",
  "studio-close-request",
  "studio-desktop-error",
  "studio-native-navigate",
] as const;
export type HostEventTopic = (typeof HOST_EVENT_TOPICS)[number];
export type HostEventEnvelope = { event: HostEventTopic; payload: unknown };
interface HostChannel {
  onmessage: (value: unknown) => void;
  toJSON(): string;
}
interface Host {
  core: {
    invoke: NativeInvoke;
    // The pinned Tauri global owns callback registration, order and cleanup.
    Channel: new (receive: (value: unknown) => void) => HostChannel;
  };
}

/** This entry point belongs only to the private native main document. */
export function nativeHost(): Host | undefined {
  if (
    typeof window === "undefined" ||
    window.top !== window ||
    window.parent !== window ||
    window.location?.protocol === "studio-view:"
  )
    return undefined;
  const host = (globalThis as typeof globalThis & { __TAURI__?: Host })
    .__TAURI__;
  return typeof host?.core?.invoke === "function" &&
    typeof host.core.Channel === "function"
    ? host
    : undefined;
}
type Watcher = { callback: (payload: unknown) => void };
const watchers = new Map<HostEventTopic, Set<Watcher>>();
let connection: Promise<void> | undefined;
let channel: HostChannel | undefined;
let leaseId: string | undefined;
let disposed = false;
let disposal: Promise<void> | undefined;

function receive(input: unknown) {
  if (disposed || !input || typeof input !== "object" || Array.isArray(input))
    return;
  const value = input as Partial<HostEventEnvelope>;
  if (
    Object.keys(value).length !== 2 ||
    !Object.hasOwn(value, "event") ||
    !Object.hasOwn(value, "payload") ||
    !HOST_EVENT_TOPICS.includes(value.event as HostEventTopic)
  )
    return;
  const listeners = watchers.get(value.event as HostEventTopic);
  let failed = false;
  let failure: unknown;
  for (const watcher of [...(listeners ?? [])]) {
    if (disposed || !listeners?.has(watcher)) continue;
    try {
      watcher.callback(value.payload);
    } catch (error) {
      if (!failed) {
        failed = true;
        failure = error;
      }
    }
  }
  // The Tauri Channel surfaces the error after every current UI watcher receives the event.
  if (failed) throw failure;
}

/** One connection per document. An uncertain reply never causes a reconnect. */
export function connectHostEvents(): Promise<void> {
  if (disposed)
    return Promise.reject(
      new Error("The native UI event connection is disposed."),
    );
  connection ??= (async () => {
    const host = nativeHost();
    if (!host) throw new Error("The native client is unavailable.");
    channel = new host.core.Channel(receive);
    const result = await client.nativeRequest<{ leaseId: string }>(
      HOST_EVENT_CONNECT_METHOD,
      { callback: channel.toJSON() },
    );
    if (
      !result ||
      typeof result !== "object" ||
      Array.isArray(result) ||
      Object.keys(result).length !== 1 ||
      typeof result.leaseId !== "string" ||
      !result.leaseId
    )
      throw new Error("The native UI event lease is invalid.");
    leaseId = result.leaseId;
  })();
  return connection;
}
export async function listenHostEvent(
  topic: HostEventTopic,
  callback: (payload: unknown) => void,
): Promise<() => void> {
  if (!HOST_EVENT_TOPICS.includes(topic))
    throw new Error("The native UI event topic is invalid.");
  if (disposed) throw new Error("The native UI event connection is disposed.");
  const listeners = watchers.get(topic) ?? new Set<Watcher>();
  watchers.set(topic, listeners);
  const watcher = { callback };
  listeners.add(watcher);
  const stop = () => {
    listeners.delete(watcher);
  };
  try {
    await connectHostEvents();
    if (disposed)
      throw new Error("The native UI event connection is disposed.");
  } catch (error) {
    stop();
    throw error;
  }
  return stop;
}

/** Release only this document's UI watchers. Business commands keep their original lifetime. */
export function disposeHostClient(): Promise<void> {
  disposal ??= (async () => {
    disposed = true;
    for (const listeners of watchers.values()) listeners.clear();
    watchers.clear();
    if (channel) channel.onmessage = () => {};
    if (!connection) return;
    await connection;
    const result = await client.nativeRequest<{ disconnected: boolean }>(
      HOST_EVENT_DISCONNECT_METHOD,
      { leaseId },
    );
    if (
      !result ||
      typeof result !== "object" ||
      Array.isArray(result) ||
      Object.keys(result).length !== 1 ||
      typeof result.disconnected !== "boolean"
    )
      throw new Error("The native UI event disconnect response is invalid.");
  })();
  return disposal;
}

const client = createNativeClient({
  available: () => nativeHost() !== undefined,
  async send<T>(method: string, params: unknown, id: string): Promise<T> {
    const host = nativeHost();
    if (!host) throw new Error("The native client is unavailable.");
    return invokeClientRequest<T>(
      (command, args) => host.core.invoke(command, args),
      method,
      params,
      id,
    );
  },
  listen: (callback) => listenHostEvent("studio-client-event", callback),
});
export const {
  hasNativeClient,
  nativeRequest,
  listenNativeEvents,
  nativeIdentity,
  prepareLegacyNativePreferences,
  nativeStorageCutover,
  nativeServerContext,
} = client;

import { SignedEventSource } from "../servers/eventSource";
import {
  API_SCHEMA_HASH,
  API_SCHEMA_HASH_HEADER,
  API_SCHEMA_HASH_PARAM,
} from "../generated/apiSchema";
import {
  initializePreferenceMigration,
  readPreferenceFields,
  receivePreferenceFields,
  watchPreferenceWrites,
} from "./uiPreferenceStore";
import {
  canonicalPreferenceJson,
  type PreferenceFields,
  type PreferenceScope,
} from "./uiPreferenceMerge";
import type { components } from "../generated/api";
type Pull = components["schemas"]["SyncPullResponse"];
type Fetch = (request: Request) => Promise<Response>;

/** Uses the existing entity pull and state resource stream for each paired server. */
export function connectUiPreferences(
  origin: string,
  fetchRequest: Fetch,
  storage: Storage,
  onIdentity?: (workspaceId: string) => void,
) {
  let stopped = false;
  let ready = false;
  let cursor = 0;
  let pulling = false;
  let pullAgain = false;
  let sending = false;
  let sendAgain = false;
  let stream: SignedEventSource | undefined;
  let retry: ReturnType<typeof setTimeout> | undefined;
  const controller = new AbortController();
  const confirmed: Partial<Record<PreferenceScope, string>> = {};
  initializePreferenceMigration("user", storage);
  initializePreferenceMigration("server", storage);
  let token = "";
  const request = async (path: string, body?: unknown) => {
    let requestId = "";
    if (body) {
      const hash = await crypto.subtle.digest(
        "SHA-256",
        new TextEncoder().encode(canonicalPreferenceJson(body)),
      );
      requestId =
        "ui-preferences:" +
        Array.from(new Uint8Array(hash), (byte) =>
          byte.toString(16).padStart(2, "0"),
        ).join("");
    }
    const response = await fetchRequest(
      new Request(origin + path, {
        headers: {
          [API_SCHEMA_HASH_HEADER]: API_SCHEMA_HASH,
          ...(body
            ? {
                "Content-Type": "application/json",
                "X-Canvas-Token": token,
                "X-Studio-Request-Id": requestId,
              }
            : {}),
        },
        method: body ? "POST" : "GET",
        ...(body ? { body: JSON.stringify(body) } : {}),
        signal: controller.signal,
        redirect: "error",
        cache: "no-store",
      }),
    );
    if (
      !response.ok ||
      response.headers.get(API_SCHEMA_HASH_HEADER) !== API_SCHEMA_HASH
    )
      throw new Error("Preferences are unavailable.");
    return response.json();
  };
  const scheduleRetry = () => {
    if (stopped || retry) return;
    retry = setTimeout(() => {
      retry = undefined;
      void pull().then(openStream).catch(scheduleRetry);
    }, 3000);
  };
  const send = async () => {
    if (!ready || stopped) return;
    if (sending) {
      sendAgain = true;
      return;
    }
    sending = true;
    try {
      token = (await request("/api/session")).token;
      do {
        sendAgain = false;
        for (const scope of ["user", "server"] as const) {
          const fields = readPreferenceFields(scope, storage);
          const serialized = JSON.stringify(fields);
          if (!Object.keys(fields).length || confirmed[scope] === serialized)
            continue;
          const result = await request("/api/sync/preferences", {
            scope,
            fields,
          });
          if (stopped) return;
          confirmed[scope] = serialized;
          receivePreferenceFields(
            scope,
            result.fields as PreferenceFields,
            storage,
          );
        }
      } while (sendAgain && !stopped);
    } catch {
      scheduleRetry();
    } finally {
      sending = false;
    }
  };
  const pull = async () => {
    if (stopped) return;
    if (pulling) {
      pullAgain = true;
      return;
    }
    pulling = true;
    try {
      do {
        pullAgain = false;
        let more: boolean;
        do {
          const response = (await request(
            `/api/sync/pull?scope=state:entities:v1&after=${cursor}&limit=500`,
          )) as Pull & { reset?: boolean };
          if (stopped) return;
          onIdentity?.(response.workspaceId);
          if (response.reset) {
            cursor = 0;
            more = true;
            continue;
          }
          for (const document of response.documents) {
            if (document._deleted) continue;
            const entity = JSON.parse(document.payload);
            if (
              entity.collection === "uiPreferences" &&
              ["user", "server"].includes(entity.id)
            ) {
              receivePreferenceFields(entity.id, entity.value.fields, storage);
              confirmed[entity.id as PreferenceScope] = JSON.stringify(
                entity.value.fields,
              );
            }
          }
          cursor = response.checkpoint.seq;
          more = cursor < (response.maxSeq ?? cursor);
        } while (more && !stopped);
      } while (pullAgain && !stopped);
      ready = true;
      await send();
    } finally {
      pulling = false;
    }
  };
  const openStream = () => {
    if (stopped || stream) return;
    const query = new URLSearchParams({
      protocol: "3",
      resources: JSON.stringify([{ kind: "state" }]),
      [API_SCHEMA_HASH_PARAM]: API_SCHEMA_HASH,
    });
    stream = new SignedEventSource(
      origin + `/api/sync/stream?${query}`,
      fetchRequest,
    );
    stream.addEventListener("resources", () => {
      void pull().catch(scheduleRetry);
    });
    stream.onerror = () => {
      stream?.close();
      stream = undefined;
      scheduleRetry();
    };
  };
  const stopWrites = watchPreferenceWrites(() => {
    void send();
  });
  const resume = () => {
    void pull().then(openStream).catch(scheduleRetry);
  };
  window.addEventListener("online", resume);
  window.addEventListener("focus", resume);
  void pull().then(openStream).catch(scheduleRetry);
  return () => {
    stopped = true;
    controller.abort();
    stream?.close();
    clearTimeout(retry);
    stopWrites();
    window.removeEventListener("online", resume);
    window.removeEventListener("focus", resume);
  };
}

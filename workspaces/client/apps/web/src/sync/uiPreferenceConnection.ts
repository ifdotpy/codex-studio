import { SignedEventSource } from "../servers/eventSource";
import {
  API_SCHEMA_HASH,
  API_SCHEMA_HASH_HEADER,
  API_SCHEMA_HASH_PARAM,
} from "../generated/apiSchema";
import {
  ApiSchemaMismatchError,
  isApiSchemaMismatchHandshake,
  isApiSchemaMismatchResponse,
  markApiSchemaMismatch,
  onApiSchemaMismatch,
} from "../api";
import { apiOrigin } from "../servers/transport";
import {
  initializePreferenceMigration,
  acknowledgePreferenceFields,
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
type PreferenceChangeEvent = components["schemas"]["ResourceChangeEvent"];

function preferenceScopeFromEntityId(id: string): PreferenceScope | undefined {
  if (id === "entity:uiPreferences:user") return "user";
  if (id === "entity:uiPreferences:server") return "server";
  return undefined;
}

/** Uses the existing entity pull and state resource stream for each paired server. */
export function connectUiPreferences(
  origin: string,
  fetchRequest: Fetch,
  storage: Storage,
  onIdentity?: (workspaceId: string) => void,
  onSchemaMismatch?: () => void,
) {
  let stopped = false;
  let ready = false;
  let cursor = 0;
  let workspaceId: string | undefined;
  let pulling = false;
  let pullAgain = false;
  let sending = false;
  let sendAgain = false;
  let stream: SignedEventSource | undefined;
  let retry: ReturnType<typeof setTimeout> | undefined;
  const controller = new AbortController();
  let stopWrites = () => {};
  let stopGateListener = () => {};
  let resume = () => {};
  const stop = () => {
    if (stopped) return;
    stopped = true;
    controller.abort();
    stream?.close();
    stream = undefined;
    if (retry) clearTimeout(retry);
    retry = undefined;
    stopWrites();
    stopGateListener();
    window.removeEventListener("online", resume);
    window.removeEventListener("focus", resume);
  };
  const stopForSchemaMismatch = () => {
    if (stopped) return;
    stop();
    onSchemaMismatch?.();
  };
  const fetchWithSchemaGate = async (request: Request) => {
    if (stopped) throw new ApiSchemaMismatchError();
    let response: Response;
    try {
      response = await fetchRequest(request);
    } catch (error) {
      if (error instanceof ApiSchemaMismatchError) stopForSchemaMismatch();
      throw error;
    }
    if (isApiSchemaMismatchResponse(response, request)) {
      stopForSchemaMismatch();
      throw new ApiSchemaMismatchError(true);
    }
    return response;
  };
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
    const outgoing = new Request(origin + path, {
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
    });
    if (stopped) throw new ApiSchemaMismatchError();
    const response = await fetchWithSchemaGate(outgoing);
    if (!response.ok) throw new Error("Preferences are unavailable.");
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
    const pendingFields = () =>
      (["user", "server"] as const)
        .map((scope) => {
          const fields = readPreferenceFields(scope, storage);
          return {
            scope,
            fields,
            serialized: JSON.stringify(fields),
          };
        })
        .filter(
          ({ scope, fields, serialized }) =>
            Object.keys(fields).length > 0 && confirmed[scope] !== serialized,
        );
    if (!pendingFields().length) return;
    sending = true;
    try {
      token = (await request("/api/session")).token;
      do {
        sendAgain = false;
        for (const { scope, fields, serialized } of pendingFields()) {
          const result = await request("/api/sync/preferences", {
            scope,
            fields,
          });
          if (stopped) return;
          confirmed[scope] = serialized;
          acknowledgePreferenceFields(
            scope,
            fields,
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
          workspaceId = response.workspaceId;
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
              confirmed[entity.id as PreferenceScope] = JSON.stringify(
                entity.value.fields,
              );
              receivePreferenceFields(entity.id, entity.value.fields, storage);
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
      fetchWithSchemaGate,
    );
    stream.addEventListener("api-schema", (event) => {
      let payload: unknown;
      try {
        payload = JSON.parse((event as MessageEvent<string>).data);
      } catch {
        payload = null;
      }
      if (!isApiSchemaMismatchHandshake(payload)) return;
      if (new URL(origin).origin === apiOrigin()) {
        const serverHash =
          payload && typeof payload === "object"
            ? (payload as { hash?: unknown }).hash
            : undefined;
        markApiSchemaMismatch(serverHash);
      }
      stopForSchemaMismatch();
    });
    stream.addEventListener("resources", (event) => {
      let payload: unknown;
      try {
        payload = JSON.parse((event as MessageEvent<string>).data);
      } catch {
        payload = null;
      }
      if (!payload || typeof payload !== "object") {
        void pull().catch(scheduleRetry);
        return;
      }
      const change = payload as PreferenceChangeEvent;
      if (change.reason !== "change") {
        void pull().catch(scheduleRetry);
        return;
      }
      if (
        !Array.isArray(change.resources) ||
        !Array.isArray(change.resourceVersions) ||
        change.resources.length !== change.resourceVersions.length ||
        change.workspaceId !== workspaceId
      ) {
        void pull().catch(scheduleRetry);
        return;
      }
      let nextCursor = cursor;
      const preferenceRows: {
        scope: PreferenceScope;
        fields: PreferenceFields;
      }[] = [];
      for (const [index, version] of change.resourceVersions.entries()) {
        const resource = version.resource ?? change.resources[index];
        if (!resource) {
          void pull().catch(scheduleRetry);
          return;
        }
        if (resource.kind !== "state") continue;
        const batch = version.entityChanges;
        if (
          version.entitySequenceReset ||
          !batch ||
          batch.after !== nextCursor ||
          batch.through !== version.revision
        ) {
          void pull().catch(scheduleRetry);
          return;
        }
        for (const document of batch.documents) {
          const scope = preferenceScopeFromEntityId(document.id);
          if (!scope) continue;
          if (document._deleted) {
            void pull().catch(scheduleRetry);
            return;
          }
          try {
            const entity = JSON.parse(document.payload);
            if (
              entity.collection !== "uiPreferences" ||
              entity.id !== scope ||
              !entity.value ||
              typeof entity.value.fields !== "object" ||
              entity.value.fields === null
            ) {
              void pull().catch(scheduleRetry);
              return;
            }
            preferenceRows.push({
              scope,
              fields: entity.value.fields as PreferenceFields,
            });
          } catch {
            void pull().catch(scheduleRetry);
            return;
          }
        }
        nextCursor = batch.through;
      }
      for (const { scope, fields } of preferenceRows) {
        confirmed[scope] = JSON.stringify(fields);
        receivePreferenceFields(scope, fields, storage);
      }
      cursor = nextCursor;
    });
    stream.onerror = () => {
      stream?.close();
      stream = undefined;
      scheduleRetry();
    };
  };
  stopWrites = watchPreferenceWrites(() => {
    void send();
  });
  resume = () => {
    void pull().then(openStream).catch(scheduleRetry);
  };
  window.addEventListener("online", resume);
  window.addEventListener("focus", resume);
  if (new URL(origin).origin === apiOrigin()) {
    stopGateListener = onApiSchemaMismatch(stopForSchemaMismatch);
    if (stopped) stopGateListener();
  }
  if (!stopped) void pull().then(openStream).catch(scheduleRetry);
  return stop;
}

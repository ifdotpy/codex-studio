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
import { EntitySequenceCheckpoint } from "./entitySequence";
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
  // Share the main sync client's epoch/reset/coverage rules. This connector
  // advances it only after a pull or a complete state change batch is applied.
  const cursor = new EntitySequenceCheckpoint();
  let workspaceId: string | undefined;
  // Pull triggers are uncovered baselines, invalid/non-contiguous state frames,
  // sequence hints, unknown stream/open failures, and exhausted read retries;
  // each uses reconcile's coverage rule. Focus/online reopen and use the baseline.
  let pulling = false;
  let pendingReconciliation:
    | { sequence?: number; unknown: boolean }
    | undefined;
  let sending = false;
  let sendAgain = false;
  let stream: SignedEventSource | undefined;
  let streamOpen = false;
  let wasOffline = false;
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
    window.removeEventListener("offline", markOffline);
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
      if (stream) void reconcile().catch(scheduleRetry);
      else openStream();
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
  const performPull = async () => {
    if (stopped) return;
    let more: boolean;
    do {
      const response = (await request(
        `/api/sync/pull?scope=state:entities:v1&after=${Math.max(0, cursor.value)}&limit=500`,
      )) as Pull & { reset?: boolean };
      if (stopped) return;
      workspaceId = response.workspaceId;
      onIdentity?.(response.workspaceId);
      if (response.reset) {
        cursor.reset();
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
      cursor.assign(response.checkpoint.seq);
      more = cursor.value < (response.maxSeq ?? cursor.value);
    } while (more && !stopped);
  };
  const queueReconciliation = (hint?: number) => {
    if (hint === undefined || !Number.isSafeInteger(hint)) {
      pendingReconciliation = { unknown: true };
      return;
    }
    if (pendingReconciliation?.unknown) return;
    pendingReconciliation = {
      unknown: false,
      sequence: Math.max(pendingReconciliation?.sequence ?? hint, hint),
    };
  };
  const reconcile = async (hint?: number) => {
    if (stopped) return;
    const needsPull = (sequence?: number) =>
      sequence === undefined ||
      !cursor.canUseDurableCheckpoint ||
      !cursor.covers(sequence);
    if (pulling) {
      if (needsPull(hint)) queueReconciliation(hint);
      return;
    }
    if (!needsPull(hint)) {
      ready = true;
      await send();
      return;
    }
    pulling = true;
    try {
      let nextHint = hint;
      do {
        if (needsPull(nextHint)) await performPull();
        const pending = pendingReconciliation;
        pendingReconciliation = undefined;
        if (!pending) break;
        nextHint = pending.unknown ? undefined : pending.sequence;
      } while (!stopped);
      if (stopped) return;
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
    let openedStream: SignedEventSource;
    try {
      openedStream = new SignedEventSource(
        origin + `/api/sync/stream?${query}`,
        fetchWithSchemaGate,
      );
      stream = openedStream;
      openedStream.onopen = () => {
        if (stream === openedStream) streamOpen = true;
      };
    } catch {
      void reconcile().catch(scheduleRetry);
      scheduleRetry();
      return;
    }
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
        void reconcile().catch(scheduleRetry);
        return;
      }
      const change = payload as PreferenceChangeEvent;
      streamOpen = true;
      if (change.reason !== "change") {
        if (
          typeof change.workspaceId !== "string" ||
          typeof change.epoch !== "string" ||
          !Array.isArray(change.resources)
        ) {
          void reconcile().catch(scheduleRetry);
          return;
        }
        workspaceId = change.workspaceId;
        onIdentity?.(change.workspaceId);
        const workspaceChanged = cursor.observeWorkspace(change.workspaceId);
        const epochChanged = cursor.observeEpoch(change.epoch);
        if (
          !Array.isArray(change.resourceVersions) ||
          change.resources.length !== change.resourceVersions.length
        ) {
          void reconcile().catch(scheduleRetry);
          return;
        }
        const stateVersion = change.resourceVersions.find(
          (version, index) =>
            (version.resource ?? change.resources[index])?.kind === "state",
        );
        if (!stateVersion) {
          void reconcile().catch(scheduleRetry);
          return;
        }
        if (stateVersion.entitySequenceReset) cursor.reset();
        const sequences = stateVersion.entitySequences;
        const baselineSequence =
          sequences && sequences.length > 0
            ? Math.max(...sequences)
            : stateVersion.revision;
        const hint =
          workspaceChanged ||
          epochChanged ||
          stateVersion.entitySequenceReset === true
            ? undefined
            : baselineSequence;
        void reconcile(hint).catch(scheduleRetry);
        return;
      }
      if (
        !Array.isArray(change.resources) ||
        !Array.isArray(change.resourceVersions) ||
        change.resources.length !== change.resourceVersions.length ||
        change.workspaceId !== workspaceId
      ) {
        void reconcile().catch(scheduleRetry);
        return;
      }
      let nextCursor = cursor.value;
      const preferenceRows: {
        scope: PreferenceScope;
        fields: PreferenceFields;
      }[] = [];
      for (const [index, version] of change.resourceVersions.entries()) {
        const resource = version.resource ?? change.resources[index];
        if (!resource) {
          void reconcile().catch(scheduleRetry);
          return;
        }
        if (resource.kind !== "state") continue;
        const batch = version.entityChanges;
        const hintedSequence = version.entitySequences?.length
          ? Math.max(...version.entitySequences)
          : batch?.through;
        if (
          version.entitySequenceReset ||
          !batch ||
          batch.after !== nextCursor ||
          batch.through !== version.revision
        ) {
          void reconcile(
            version.entitySequenceReset ? undefined : hintedSequence,
          ).catch(scheduleRetry);
          return;
        }
        for (const document of batch.documents) {
          const scope = preferenceScopeFromEntityId(document.id);
          if (!scope) continue;
          if (document._deleted) {
            void reconcile(batch.through).catch(scheduleRetry);
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
              void reconcile(batch.through).catch(scheduleRetry);
              return;
            }
            preferenceRows.push({
              scope,
              fields: entity.value.fields as PreferenceFields,
            });
          } catch {
            void reconcile(batch.through).catch(scheduleRetry);
            return;
          }
        }
        nextCursor = batch.through;
      }
      for (const { scope, fields } of preferenceRows) {
        confirmed[scope] = JSON.stringify(fields);
        receivePreferenceFields(scope, fields, storage);
      }
      cursor.assign(nextCursor);
    });
    stream.onerror = () => {
      if (stopped || stream !== openedStream) {
        return;
      }
      openedStream.close();
      stream = undefined;
      streamOpen = false;
      // Reconcile from the reopened stream's baseline; its sequence tells us
      // whether a pull is needed and avoids rereading an already-covered cursor.
      scheduleRetry();
    };
  };
  stopWrites = watchPreferenceWrites(() => {
    void send();
  });
  const markOffline = () => {
    wasOffline = true;
  };
  resume = () => {
    if (stopped) return;
    if (retry && !wasOffline) return;
    if (retry) {
      clearTimeout(retry);
      retry = undefined;
    }
    if (stream && streamOpen && !wasOffline) return;
    stream?.close();
    stream = undefined;
    streamOpen = false;
    wasOffline = false;
    openStream();
  };
  window.addEventListener("online", resume);
  window.addEventListener("focus", resume);
  window.addEventListener("offline", markOffline);
  if (new URL(origin).origin === apiOrigin()) {
    stopGateListener = onApiSchemaMismatch(stopForSchemaMismatch);
    if (stopped) stopGateListener();
  }
  if (!stopped) openStream();
  return stop;
}

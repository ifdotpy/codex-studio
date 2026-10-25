import { errorText } from "../api";
import {
  registerPreferenceRetry,
  setPreferenceWriteStatus,
} from "./uiPreferenceStatus";
import type { syncPost } from "../api";
import { onResume } from "./resume";
import {
  acknowledgePreferenceFields,
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
import type { UiPreferenceProjection } from "./entityProjection";

/** Bridges projected preference rows to local storage and gated API writes. */
export function createUiPreferenceBridge(
  storage: Storage,
  post: typeof syncPost<"/api/sync/preferences">,
) {
  let active = false;
  let ready = false;
  let sending = false;
  let sendAgain = false;
  const confirmed: Partial<Record<PreferenceScope, string>> = {};

  const send = async () => {
    if (!active || !ready) return;
    if (sending) {
      sendAgain = true;
      return;
    }
    sending = true;
    // A failed scope waits for a later explicit edit, resume, or retry event.
    // Successful acknowledgements may request another drain, but must not spin
    // on a persistent rejection in the other scope.
    const failedScopes = new Set<PreferenceScope>();
    try {
      do {
        sendAgain = false;
        for (const scope of ["user", "server"] as const) {
          const fields = readPreferenceFields(scope, storage);
          const serialized = canonicalPreferenceJson(fields);
          if (!Object.keys(fields).length || confirmed[scope] === serialized) {
            setPreferenceWriteStatus(storage, scope, { state: "saved" });
            continue;
          }
          if (failedScopes.has(scope)) continue;
          try {
            setPreferenceWriteStatus(storage, scope, { state: "saving" });
            const body = { scope, fields };
            const digest = await crypto.subtle.digest(
              "SHA-256",
              new TextEncoder().encode(canonicalPreferenceJson(body)),
            );
            if (!active) return;
            const requestId =
              "ui-preferences:" +
              Array.from(new Uint8Array(digest), (byte) =>
                byte.toString(16).padStart(2, "0"),
              ).join("");
            const result = await post("/api/sync/preferences", body, {
              requestId,
            });
            if (!active) return;
            confirmed[scope] = canonicalPreferenceJson(result.fields);
            acknowledgePreferenceFields(
              scope,
              fields,
              result.fields as PreferenceFields,
              storage,
            );
            const pending =
              canonicalPreferenceJson(readPreferenceFields(scope, storage)) !==
              confirmed[scope];
            setPreferenceWriteStatus(storage, scope, {
              state: pending ? "saving" : "saved",
            });
            if (pending) sendAgain = true;
          } catch (error) {
            if (!active) return;
            failedScopes.add(scope);
            setPreferenceWriteStatus(storage, scope, {
              state: "error",
              error:
                errorText(error) ||
                "Could not save preferences. Check the server connection and retry.",
            });
          }
        }
      } while (sendAgain && active);
    } finally {
      sending = false;
    }
  };

  const receiveProjection = (preferences: UiPreferenceProjection) => {
    if (!active) return;
    ready = true;
    for (const scope of ["user", "server"] as const) {
      const fields = preferences[scope]?.fields as PreferenceFields | undefined;
      if (!fields) continue;
      const serialized = canonicalPreferenceJson(fields);
      if (confirmed[scope] === serialized) continue;
      confirmed[scope] = serialized;
      receivePreferenceFields(scope, fields, storage);
    }
    void send();
  };

  return {
    receiveProjection,
    retryPendingWrites: () => void send(),
    start() {
      if (active) return () => {};
      active = true;
      initializePreferenceMigration("user", storage);
      initializePreferenceMigration("server", storage);
      for (const scope of ["user", "server"] as const) {
        if (Object.keys(readPreferenceFields(scope, storage)).length)
          setPreferenceWriteStatus(storage, scope, { state: "saving" });
      }
      const stopRetry = registerPreferenceRetry(storage, () => void send());
      const stopWrites = watchPreferenceWrites(() => void send());
      const stopResume = onResume(() => void send());
      return () => {
        active = false;
        stopResume();
        stopWrites();
        stopRetry();
      };
    },
  };
}

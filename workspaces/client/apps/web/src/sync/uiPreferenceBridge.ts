import { syncPost } from "../api";
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
export function createUiPreferenceBridge(storage: Storage) {
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
    try {
      do {
        sendAgain = false;
        for (const scope of ["user", "server"] as const) {
          const fields = readPreferenceFields(scope, storage);
          const serialized = JSON.stringify(fields);
          if (!Object.keys(fields).length || confirmed[scope] === serialized)
            continue;
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
          const result = await syncPost("/api/sync/preferences", body, {
            requestId,
          });
          if (!active) return;
          confirmed[scope] = JSON.stringify(result.fields);
          acknowledgePreferenceFields(
            scope,
            fields,
            result.fields as PreferenceFields,
            storage,
          );
        }
      } while (sendAgain && active);
    } catch {
      // The API client owns schema gating and transport failures. A later
      // preference edit or bridge mount will retry pending fields.
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
      const serialized = JSON.stringify(fields);
      if (confirmed[scope] === serialized) continue;
      confirmed[scope] = serialized;
      receivePreferenceFields(scope, fields, storage);
    }
    void send();
  };

  return {
    receiveProjection,
    start() {
      if (active) return () => {};
      active = true;
      initializePreferenceMigration("user", storage);
      initializePreferenceMigration("server", storage);
      const stopWrites = watchPreferenceWrites(() => void send());
      return () => {
        active = false;
        stopWrites();
      };
    },
  };
}

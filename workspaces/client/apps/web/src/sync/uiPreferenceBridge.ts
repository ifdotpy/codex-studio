import type { syncPost } from "../api";
import { sha256Hex } from "../sha256";
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
    try {
      do {
        sendAgain = false;
        for (const scope of ["user", "server"] as const) {
          const fields = readPreferenceFields(scope, storage);
          const serialized = canonicalPreferenceJson(fields);
          if (!Object.keys(fields).length || confirmed[scope] === serialized)
            continue;
          const body = { scope, fields };
          const digest = await sha256Hex(
            new TextEncoder().encode(canonicalPreferenceJson(body)),
          );
          if (!active) return;
          const requestId = "ui-preferences:" + digest;
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
        }
      } while (sendAgain && active);
    } catch {
      // The API client owns schema gating and transport failures. A later
      // preference edit, resume or connection recovery retries pending fields.
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
      const stopWrites = watchPreferenceWrites(() => void send());
      const stopResume = onResume(() => void send());
      return () => {
        active = false;
        stopResume();
        stopWrites();
      };
    },
  };
}

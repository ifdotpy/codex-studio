import { useCallback, useSyncExternalStore } from "react";
import {
  readPreferenceWriteStatus,
  subscribePreferenceWriteStatus,
} from "../sync/uiPreferenceStatus";
import type contract from "../../../../contracts/preference-storage.json";

/** Display native or browser receipts. This component never sends or retries. */
export default function PreferenceSyncStatus({
  storage,
  scope,
}: {
  storage: Storage;
  scope: keyof typeof contract.scopes;
}) {
  const read = useCallback(
    () => readPreferenceWriteStatus(storage, scope),
    [storage, scope],
  );
  const status = useSyncExternalStore(
    subscribePreferenceWriteStatus,
    read,
    read,
  );
  if (status.state === "saved") return null;
  if (status.state === "saving")
    return <p role="status">Saving preferences to this server.</p>;
  return (
    <p className="studio-preferences-error" role="alert">
      {status.error}
      {status.uncertain ? " The server has not confirmed this request." : ""}
    </p>
  );
}

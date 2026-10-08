import { Switch } from "@mantine/core";
import { ActionButton, SettingsRow } from "../components/ui/primitives";
import type { DiscoveryActions, DiscoverySnapshot } from "./discoveryModel";
export default function DiscoveryPanel({
  snapshot,
  busy,
  error,
  actions,
}: {
  snapshot: DiscoverySnapshot | null;
  busy: boolean;
  error: string;
  actions: DiscoveryActions;
}) {
  return (
    <section aria-label="Discovered Studio servers">
      <SettingsRow
        label={
          <Switch
            label="Pair servers automatically"
            checked={snapshot?.autoPair ?? false}
            disabled={busy || !snapshot}
            onChange={(event) =>
              actions.setAutoPair(event.currentTarget.checked)
            }
          />
        }
        help="Same Tailscale user. Existing pairs stay active."
      >
        <ActionButton
          actionRole="secondary"
          loading={busy}
          onClick={actions.find}
        >
          Find servers now
        </ActionButton>
      </SettingsRow>
      {error && (
        <p className="studio-preferences-error" role="alert">
          {error}
        </p>
      )}
      {!snapshot && !error && <p className="ui-help">Load the server list.</p>}
    </section>
  );
}

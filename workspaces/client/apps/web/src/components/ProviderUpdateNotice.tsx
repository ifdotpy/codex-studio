import type { components } from "../generated/api";

type Notice = components["schemas"]["ProviderUpdateNotice"];

export default function ProviderUpdateNotice({
  notice,
  providerLabel,
  provider,
  installedVersion,
}: {
  notice: Notice | null | undefined;
  providerLabel: string;
  provider: string;
  installedVersion: string | null;
}) {
  if (!notice) return null;
  return (
    <p role="status" className="native-runtime-warning">
      {provider === "codex"
        ? `A newer installed ${providerLabel} version is selected for runtime updates, but is not in use on this connection yet.`
        : `A newer installed ${providerLabel} version is available but not in use yet.`}{" "}
      Current: {installedVersion || "Unknown"} · Available:{" "}
      {notice.availableVersion}
    </p>
  );
}

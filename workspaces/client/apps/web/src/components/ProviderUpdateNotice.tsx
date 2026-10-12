import type { components } from "../generated/api";

type Notice = components["schemas"]["ProviderUpdateNotice"];

const providerLabels: Record<string, string> = {
  codex: "Codex CLI",
  claude: "Claude Code",
};

export default function ProviderUpdateNotice({
  notice,
}: {
  notice: Notice | null | undefined;
}) {
  if (!notice) return null;
  const provider = providerLabels[notice.provider] || notice.provider;
  return (
    <p role="status" className="native-runtime-warning">
      A newer installed {provider} version is available but not in use yet.
      Current: {notice.installedVersion} · Available: {notice.availableVersion}
    </p>
  );
}

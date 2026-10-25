import type { ServerAccount } from "./navigation";
import type { StudioServer } from "./registry";

export type ServerAccountRow = {
  key: string;
  provider: "codex" | "claude";
  email: string | null;
  label: string;
  plan: string | null;
  byServer: Record<string, ServerAccount>;
  serverScoped?: boolean;
};

/** Merge email copies only when each host has one matching profile. Unknown
 * emails and duplicate profiles stay on their own server. */
export function serverAccountRows(
  servers: StudioServer[],
  accountsByServer: Record<string, ServerAccount[]>,
): ServerAccountRow[] {
  const grouped = new Map<
    string,
    { serverId: string; account: ServerAccount }[]
  >();
  for (const server of servers) {
    for (const account of accountsByServer[server.id] || []) {
      const key = account.email
        ? `${account.provider}:${account.email.toLocaleLowerCase()}`
        : `${account.provider}:${server.id}:${account.label.toLocaleLowerCase()}`;
      const profiles = grouped.get(key) || [];
      profiles.push({ serverId: server.id, account });
      grouped.set(key, profiles);
    }
  }
  const rows: ServerAccountRow[] = [];
  for (const [key, profiles] of grouped) {
    const hosts = new Set<string>();
    const ambiguous = profiles.some(({ serverId }) => {
      if (hosts.has(serverId)) return true;
      hosts.add(serverId);
      return false;
    });
    if (ambiguous) {
      profiles.forEach(({ serverId, account }, index) => {
        // A legacy position is only a render key. It never becomes an account ID.
        const profile = account.accountKey ?? { legacyPosition: index };
        rows.push({
          key: `${key}:profile:${JSON.stringify([serverId, profile])}`,
          provider: account.provider,
          email: account.email,
          label: account.label,
          plan: account.plan,
          byServer: { [serverId]: account },
          serverScoped: true,
        });
      });
    } else {
      const account = profiles[0]!.account;
      rows.push({
        key,
        provider: account.provider,
        email: account.email,
        label: account.label,
        plan: profiles.reduce(
          (plan, profile) => plan || profile.account.plan,
          account.plan,
        ),
        byServer: Object.fromEntries(
          profiles.map((profile) => [profile.serverId, profile.account]),
        ),
      });
    }
  }
  return rows;
}

/** The servers whose chip a row shows: every server for a known person, only
 * its own server for an account without an email. */
export const rowServers = (row: ServerAccountRow, servers: StudioServer[]) =>
  row.email && !row.serverScoped
    ? servers
    : servers.filter((server) => row.byServer[server.id]);

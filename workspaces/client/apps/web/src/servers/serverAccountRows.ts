import type { ServerAccount } from "./navigation";
import type { StudioServer } from "./registry";

export type ServerAccountRow = {
  key: string;
  provider: "codex" | "claude";
  email: string | null;
  label: string;
  plan: string | null;
  byServer: Record<string, ServerAccount>;
};

/** One row per person and provider across servers. An account whose email the
 * server cannot read (for example a Claude sign-in that the Keychain blocks)
 * has no known identity: it stays a row of its own server, so it never merges
 * with another account and is never offered to another server. */
export function serverAccountRows(
  servers: StudioServer[],
  accountsByServer: Record<string, ServerAccount[]>,
): ServerAccountRow[] {
  const grouped = new Map<string, ServerAccountRow>();
  for (const server of servers) {
    for (const account of accountsByServer[server.id] || []) {
      const key = account.email
        ? `${account.provider}:${account.email.toLocaleLowerCase()}`
        : `${account.provider}:${server.id}:${account.label.toLocaleLowerCase()}`;
      const row = grouped.get(key) || {
        key,
        provider: account.provider,
        email: account.email,
        label: account.label,
        plan: account.plan,
        byServer: {},
      };
      row.byServer[server.id] = account;
      if (!row.plan) row.plan = account.plan;
      grouped.set(key, row);
    }
  }
  return [...grouped.values()];
}

/** The servers whose chip a row shows: every server for a known person, only
 * its own server for an account without an email. */
export const rowServers = (row: ServerAccountRow, servers: StudioServer[]) =>
  row.email ? servers : servers.filter((server) => row.byServer[server.id]);

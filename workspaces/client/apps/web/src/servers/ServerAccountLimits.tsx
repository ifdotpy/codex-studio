import { useEffect, useRef, useState, type ReactNode } from "react";
import { errorText } from "../api";
import AccountStartThrottle from "../components/AccountStartThrottle";
import { accountLimitView } from "../components/AccountTiles";
import { AccountCapacityView, type Account } from "../components/Accounts";
import type { AccountLimitsSnapshot } from "../usage/accountUsage";
import { useAccountLimits } from "../usage/useAccountLimits";
import { ServerServicesProvider, useServerApi } from "./ServerServicesContext";
import type { ServerAccount } from "./navigation";
import type { ServerServices } from "./serverServices";

type Accounts = Record<string, Account[]>;

/** The full accounts of every server (ids and stored limits), read from each
 * server's own API when the settings panel opens. */
export function useServerFullAccounts(
  serverIds: string[],
  servicesFor: ((serverId: string) => ServerServices | undefined) | undefined,
) {
  const [accounts, setAccounts] = useState<Accounts>({});
  const fallback = useServerApi();
  const latest = useRef({ servicesFor, fallback });
  latest.current = { servicesFor, fallback };
  const key = serverIds.join("\n");
  useEffect(() => {
    let stopped = false;
    for (const id of key ? key.split("\n") : []) {
      const { servicesFor, fallback } = latest.current;
      void (servicesFor?.(id)?.api ?? fallback)
        .get("/api/accounts")
        .then((result) => {
          if (!stopped)
            setAccounts((old) => ({ ...old, [id]: result.accounts }));
        })
        .catch((failure: unknown) => console.warn(errorText(failure)));
    }
    return () => {
      stopped = true;
    };
  }, [key]);
  return [accounts, setAccounts] as const;
}

/** The server's own account that a panel row stands for. */
export const matchServerAccount = (
  accounts: Account[] | undefined,
  row: Pick<ServerAccount, "provider" | "email" | "label">,
) =>
  accounts?.find(
    (account) =>
      account.provider === row.provider &&
      (row.email
        ? account.email?.toLocaleLowerCase() === row.email.toLocaleLowerCase()
        : account.label === row.label),
  );

/** Requests in this subtree go to the given server, or to the default API. */
function ServerScope({
  services,
  children,
}: {
  services?: ServerServices;
  children: ReactNode;
}) {
  return services ? (
    <ServerServicesProvider services={services}>
      {children}
    </ServerServicesProvider>
  ) : (
    <>{children}</>
  );
}

/** One server's copy of a provider account. */
export type AccountSource = {
  serverId: string;
  serverLabel: string;
  account: Account;
  services?: ServerServices;
};
type Reading = AccountSource & { limits: AccountLimitsSnapshot | null };

/** The reading that the account card shows. Usage limits belong to the
 * provider account, so every server reads the same limit: the card shows the
 * reading with data whose `at` (epoch seconds) is newest. Without any data it
 * shows a read in progress, then a read error, then an answer without data. */
export function newestReading<
  T extends { limits: AccountLimitsSnapshot | null },
>(readings: T[]): T | undefined {
  let newest: T | undefined;
  for (const reading of readings)
    if (
      reading.limits?.data &&
      (!newest || (reading.limits.at || 0) > (newest.limits!.at || 0))
    )
      newest = reading;
  return (
    newest ??
    readings.find((reading) => !reading.limits) ??
    readings.find((reading) => reading.limits?.error) ??
    readings[0]
  );
}

/** "just now", "5 min ago", "3 h ago", "2 d ago" for an age in seconds. */
export function readingAge(seconds: number) {
  if (seconds < 60) return "just now";
  if (seconds < 3600) return `${Math.floor(seconds / 60)} min ago`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)} h ago`;
  return `${Math.floor(seconds / 86400)} d ago`;
}

function SourceReading({
  source,
  onReading,
}: {
  source: AccountSource;
  onReading: (serverId: string, limits: AccountLimitsSnapshot | null) => void;
}) {
  const limits = useAccountLimits(
    source.account.id,
    source.account.accountId,
    source.account.status === "ready",
  );
  const report = useRef(onReading);
  report.current = onReading;
  useEffect(() => {
    report.current(source.serverId, limits);
  }, [source.serverId, limits]);
  return null;
}

/** How often, in milliseconds, the shown reading age is computed again. */
const AGE_REFRESH_MS = 30_000;

/** The remaining limit of one provider account, shown once for all servers
 * where it is signed in. Each server reads through its own API and cache. */
export function AccountLimitsAcrossServers({
  sources,
}: {
  sources: AccountSource[];
}) {
  const [limits, setLimits] = useState<
    Record<string, AccountLimitsSnapshot | null>
  >({});
  const [now, setNow] = useState(() => Date.now() / 1000);
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now() / 1000), AGE_REFRESH_MS);
    return () => clearInterval(timer);
  }, []);
  const ready = sources.filter((source) => source.account.status === "ready");
  const readings: Reading[] = ready.map((source) => ({
    ...source,
    limits: limits[source.serverId] ?? null,
  }));
  const shown = newestReading(readings);
  const account = shown?.account ?? sources[0]?.account;
  if (!account) return null;
  const at = shown?.limits?.data ? shown.limits.at : null;
  const note =
    typeof at === "number"
      ? `Updated ${readingAge(Math.max(0, now - at))}${
          sources.length > 1 ? ` on ${shown!.serverLabel}` : ""
        }`
      : undefined;
  return (
    <>
      {ready.map((source) => (
        <ServerScope key={source.serverId} services={source.services}>
          <SourceReading
            source={source}
            onReading={(serverId, next) =>
              setLimits((old) =>
                old[serverId] === next ? old : { ...old, [serverId]: next },
              )
            }
          />
        </ServerScope>
      ))}
      <AccountCapacityView
        card
        account={account}
        view={accountLimitView(account, shown?.limits ?? null, now)}
        note={note}
      />
    </>
  );
}

/** Turn start limits are a setting of one Studio server, stored on that
 * server's account, so the request goes to that server. */
export function ServerTurnStartLimits({
  source,
  onSaved,
}: {
  source: AccountSource;
  onSaved: (accounts: Account[]) => void;
}) {
  return (
    <ServerScope services={source.services}>
      <AccountStartThrottle
        account={source.account}
        serverLabel={source.serverLabel}
        onSaved={(state) => onSaved(state.accounts)}
      />
    </ServerScope>
  );
}

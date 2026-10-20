import { useEffect, useRef, useState } from "react";
import { errorText } from "../api";
import AccountStartThrottle from "../components/AccountStartThrottle";
import { AccountCapacity, type Account } from "../components/Accounts";
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

/** One account on one server: its remaining capacity and turn start limits.
 * Limits are stored per server account, so the request goes to that server. */
export default function ServerAccountLimits({
  account,
  services,
  onSaved,
}: {
  account: Account;
  services?: ServerServices;
  onSaved: (accounts: Account[]) => void;
}) {
  const body = (
    <>
      <AccountCapacity card account={account} opened />
      <AccountStartThrottle
        account={account}
        onSaved={(state) => onSaved(state.accounts)}
      />
    </>
  );
  return services ? (
    <ServerServicesProvider services={services}>{body}</ServerServicesProvider>
  ) : (
    body
  );
}

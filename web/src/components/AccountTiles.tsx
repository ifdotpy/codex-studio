import { SegmentedControl, Tooltip } from "@mantine/core";
import { Check } from "lucide-react";
import { useEffect, useState } from "react";
import { ActionButton } from "./ui/primitives";
import { get } from "../api";
import { accountDisplayName, accountTooltip } from "../accountName";
import { accountLimits } from "../usage/accountUsage";
import { readBuckets } from "./Usage";
import { watchResourceReads } from "./watchResourceReads";
import type { Account } from "./Accounts";
import "./account-tiles.css";
export function setupAccountName(account?: Account) {
  return account ? accountDisplayName(account) : "";
}
export function ProviderMark({ provider }: { provider: string }) {
  return provider === "claude" ? (
    <svg
      width="17"
      height="17"
      viewBox="0 0 24 24"
      aria-hidden="true"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.9"
      strokeLinecap="round"
    >
      {[0, 30, 60, 90, 120, 150].map((angle) => (
        <path key={angle} d="M12 2v20" transform={`rotate(${angle} 12 12)`} />
      ))}
    </svg>
  ) : (
    <svg
      width="17"
      height="17"
      viewBox="0 0 24 24"
      aria-hidden="true"
      fill="currentColor"
    >
      <path d="M22.2819 9.8211a5.9847 5.9847 0 0 0-.5157-4.9108 6.0462 6.0462 0 0 0-6.5098-2.9A6.0651 6.0651 0 0 0 4.9807 4.1818a5.9847 5.9847 0 0 0-3.9977 2.9 6.0462 6.0462 0 0 0 .7427 7.0966 5.98 5.98 0 0 0 .511 4.9107 6.051 6.051 0 0 0 6.5146 2.9001A5.9847 5.9847 0 0 0 13.2599 24a6.0557 6.0557 0 0 0 5.7718-4.2058 5.9894 5.9894 0 0 0 3.9977-2.9001 6.0557 6.0557 0 0 0-.7475-7.0729zm-9.022 12.6081a4.4755 4.4755 0 0 1-2.8764-1.0408l.1419-.0804 4.7783-2.7582a.7948.7948 0 0 0 .3927-.6813v-6.7369l2.02 1.1686a.071.071 0 0 1 .038.052v5.5826a4.504 4.504 0 0 1-4.4945 4.4944zm-9.6607-4.1254a4.4708 4.4708 0 0 1-.5346-3.0137l.142.0852 4.783 2.7582a.7712.7712 0 0 0 .7806 0l5.8428-3.3685v2.3324a.0804.0804 0 0 1-.0332.0615L9.74 19.9502a4.4992 4.4992 0 0 1-6.1408-1.6464zM2.3408 7.8956a4.485 4.485 0 0 1 2.3655-1.9728V11.6a.7664.7664 0 0 0 .3879.6765l5.8144 3.3543-2.0201 1.1685a.0757.0757 0 0 1-.071 0l-4.8303-2.7865A4.504 4.504 0 0 1 2.3408 7.872zm16.5963 3.8558L13.1038 8.364 15.1192 7.2a.0757.0757 0 0 1 .071 0l4.8303 2.7913a4.4944 4.4944 0 0 1-.6765 8.1042v-5.6772a.79.79 0 0 0-.407-.667zm2.0107-3.0231l-.142-.0852-4.7735-2.7818a.7759.7759 0 0 0-.7854 0L9.409 9.2297V6.8974a.0662.0662 0 0 1 .0284-.0615l4.8303-2.7866a4.4992 4.4992 0 0 1 6.6802 4.66zM8.3065 12.863l-2.02-1.1638a.0804.0804 0 0 1-.038-.0567V6.0742a4.4992 4.4992 0 0 1 7.3757-3.4537l-.142.0805L8.704 5.459a.7948.7948 0 0 0-.3927.6813zm1.0976-2.3654l2.602-1.4998 2.6069 1.4998v2.9994l-2.5974 1.4997-2.6067-1.4997Z" />
    </svg>
  );
}
export function remainingLimit(
  windows: { remaining: number | null; expired: boolean }[],
) {
  const values = windows
    .filter((window) => !window.expired && window.remaining !== null)
    .map((window) => window.remaining!);
  return values.length ? Math.max(0, Math.min(100, ...values)) : null;
}
function AccountTile({
  account,
  selected,
  disabled,
  onSelect,
  multiple = false,
}: {
  account: Account;
  selected: boolean;
  disabled: boolean;
  onSelect: () => void;
  multiple?: boolean;
}) {
  const [remaining, setRemaining] = useState<number | null>(null);
  useEffect(() => {
    setRemaining(null);
    return watchResourceReads(
      { kind: "limits", accountKey: account.id },
      async () => {
        const result = await get("/api/limits", {
          query: { account_key: account.id },
          timeoutMs: 25000,
        });
        const snapshot = accountLimits(result, account.id, account.accountId);
        if (!snapshot || snapshot.error) {
          setRemaining(null);
          return;
        }
        const buckets = readBuckets(snapshot, Date.now() / 1000);
        setRemaining(
          remainingLimit(buckets.flatMap((bucket) => bucket.windows)),
        );
      },
      () => setRemaining(null),
    );
  }, [account.id, account.accountId]);
  return (
    <Tooltip label={accountTooltip(account)}>
      <ActionButton
        className="setup-account"
        data-account-key={account.id}
        data-selected={selected || undefined}
        data-exhausted={remaining === 0 || undefined}
        aria-label={`Select account ${setupAccountName(account)}`}
        aria-description={
          remaining === null ? undefined : `${remaining}% remaining`
        }
        aria-pressed={selected}
        disabled={disabled}
        onClick={onSelect}
      >
        <span>
          {multiple && selected && <Check size={13} aria-hidden="true" />}{" "}
          {setupAccountName(account)}
        </span>
        {remaining !== null && (
          <span
            className="setup-limit"
            role="meter"
            aria-label="Remaining limit"
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={remaining}
          >
            <span
              style={{
                width: `${remaining}%`,
                background:
                  remaining > 50
                    ? "var(--mantine-color-green-6)"
                    : remaining >= 15
                      ? "var(--mantine-color-yellow-6)"
                      : "var(--mantine-color-red-6)",
              }}
            />
          </span>
        )}
      </ActionButton>
    </Tooltip>
  );
}

type AccountTilesProps = {
  label: string;
  showLabel?: boolean;
  visibleLabel?: string;
  accounts: Account[];
  disabled?: boolean;
  automatic?: boolean;
  provider?: string;
  providers?: string[];
  onProvider?: (provider: string) => void;
  accountDisabled?: (account: Account) => boolean;
} & (
  | { multiple?: false; value: string; onChange: (value: string) => void }
  | { multiple: true; value: string[]; onChange: (value: string[]) => void }
);
export function AccountTiles(p: AccountTilesProps) {
  const selected = p.multiple ? p.value[0] : p.value;
  const selectedProvider =
    p.accounts.find((account) => account.id === selected)?.provider || "codex";
  const [localProvider, setLocalProvider] = useState<string>(selectedProvider);
  const provider = p.provider || localProvider;
  useEffect(() => {
    setLocalProvider(selectedProvider);
  }, [selectedProvider]);
  const providers = p.providers || [
    ...new Set(
      p.accounts
        .filter((account) => !account.disconnected && !account.deleted)
        .map((account) => account.provider || "codex"),
    ),
  ];
  const select = (key: string) => {
    if (p.multiple)
      p.onChange(
        p.value.includes(key)
          ? p.value.filter((value) => value !== key)
          : [...p.value, key],
      );
    else p.onChange(key);
  };
  return (
    <div
      className="account-tiles"
      role="group"
      aria-label={p.label}
      data-value={p.multiple ? p.value.join(",") : p.value}
      aria-disabled={p.disabled}
    >
      {p.showLabel !== false && (
        <span className="account-tiles-label">{p.visibleLabel || p.label}</span>
      )}
      {providers.length > 0 && (
        <SegmentedControl
          fullWidth
          aria-label={`${p.label} provider`}
          value={provider}
          disabled={p.disabled}
          onChange={(value) => {
            setLocalProvider(value);
            p.onProvider?.(value);
          }}
          data={providers.map((value) => ({
            value,
            label: (
              <span className="setup-provider">
                <ProviderMark provider={value} />
                {value === "claude" ? "Claude" : "Codex"}
              </span>
            ),
          }))}
        />
      )}
      <div className="setup-accounts">
        {p.automatic && (
          <ActionButton
            aria-pressed={!selected}
            data-selected={!selected || undefined}
            disabled={p.disabled}
            onClick={() => select("")}
          >
            Automatic
          </ActionButton>
        )}
        {p.accounts
          .filter((account) => (account.provider || "codex") === provider)
          .map((account) => (
            <AccountTile
              key={account.id}
              account={account}
              selected={
                p.multiple
                  ? p.value.includes(account.id)
                  : p.value === account.id
              }
              multiple={p.multiple}
              disabled={!!p.disabled || !!p.accountDisabled?.(account)}
              onSelect={() => select(account.id)}
            />
          ))}
      </div>
    </div>
  );
}

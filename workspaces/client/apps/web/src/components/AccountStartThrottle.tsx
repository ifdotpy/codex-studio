import { useServerApi } from "../servers/ServerServicesContext";
import { Button, NumberInput, Stack } from "@mantine/core";
import { useRef, useState } from "react";
import { errorText, type PostBody, type PostResult } from "../api";
import {
  ACCOUNT_RUNNING_TURNS_DEFAULT_CLAUDE,
  ACCOUNT_RUNNING_TURNS_DEFAULT_CODEX,
  ACCOUNT_RUNNING_TURNS_MAX,
  ACCOUNT_RUNNING_TURNS_MIN,
  ACCOUNT_START_BURST_DEFAULT_CLAUDE,
  ACCOUNT_START_BURST_DEFAULT_CODEX,
  ACCOUNT_START_BURST_MAX,
  ACCOUNT_START_BURST_MIN,
  ACCOUNT_START_PAUSE_DEFAULT_SECONDS_CLAUDE,
  ACCOUNT_START_PAUSE_DEFAULT_SECONDS_CODEX,
  ACCOUNT_START_PAUSE_MAX_SECONDS,
  ACCOUNT_START_PAUSE_MIN_SECONDS,
} from "../generated/sharedValues";
import type { Account } from "./Accounts";

type Throttle = Omit<PostBody<"/api/accounts/throttle">, "account_key">;

/** One account start throttle field, its generated bounds, and its help. */
export interface ThrottleFieldSpec {
  field: keyof Throttle;
  label: string;
  tooltip: string;
  min: number;
  max: number;
}

const SCOPE =
  " Providers limit requests per account. Studio counts only turn starts; " +
  "the provider's own requests inside a running turn are not limited here.";

const DEFAULTS =
  ` Defaults: Codex ${ACCOUNT_START_BURST_DEFAULT_CODEX} starts per burst, ` +
  `a ${ACCOUNT_START_PAUSE_DEFAULT_SECONDS_CODEX} s pause, ` +
  `${ACCOUNT_RUNNING_TURNS_DEFAULT_CODEX} running turns: Codex admits a burst ` +
  "of about 10 requests, then answers HTTP 429 and waits about 60 s, and the " +
  `allowance refills at about 17 requests per minute. Claude ` +
  `${ACCOUNT_START_BURST_DEFAULT_CLAUDE} starts, a ` +
  `${ACCOUNT_START_PAUSE_DEFAULT_SECONDS_CLAUDE} s pause, ` +
  `${ACCOUNT_RUNNING_TURNS_DEFAULT_CLAUDE} running turns: no Claude throttle ` +
  "was observed up to about 19 starts per minute, a burst of 12, and 15 " +
  "running turns; the running limit mainly bounds memory (about 250-350 MiB " +
  "per running Claude turn).";

export const THROTTLE_FIELDS: ThrottleFieldSpec[] = [
  {
    field: "burst",
    label: "Turn starts per burst",
    tooltip:
      "How many turns this account may start at once. After a burst, the next " +
      "start waits for the pause. Starts at least one pause apart count as " +
      "separate bursts, so occasional starts never wait. Lower it when the provider answers with rate " +
      "limit errors (HTTP 429) right after many agents start." +
      SCOPE +
      DEFAULTS,
    min: ACCOUNT_START_BURST_MIN,
    max: ACCOUNT_START_BURST_MAX,
  },
  {
    field: "pauseSeconds",
    label: "Pause after a burst (seconds)",
    tooltip:
      "How long the next start waits after the start that ends a burst (the " +
      "last start of the burst, or the start that takes the last free running " +
      "slot). Raise it when the provider still limits requests per minute; 0 " +
      "means no pause." +
      SCOPE +
      DEFAULTS,
    min: ACCOUNT_START_PAUSE_MIN_SECONDS,
    max: ACCOUNT_START_PAUSE_MAX_SECONDS,
  },
  {
    field: "maxRunningTurns",
    label: "Running turns at once",
    tooltip:
      "How many turns of this account may run at the same time. A turn over " +
      "the limit waits with its message queued and starts when a running turn " +
      "ends; it never fails. Lower it when many parallel agents hit the " +
      "provider's limits; raise it to run more agents at once on this account." +
      SCOPE +
      DEFAULTS,
    min: ACCOUNT_RUNNING_TURNS_MIN,
    max: ACCOUNT_RUNNING_TURNS_MAX,
  },
];

/** The account's stored throttle, or its provider's generated defaults. */
export function accountThrottle(account: Account): Throttle {
  if (account.startThrottle) return account.startThrottle;
  return account.provider === "claude"
    ? {
        burst: ACCOUNT_START_BURST_DEFAULT_CLAUDE,
        pauseSeconds: ACCOUNT_START_PAUSE_DEFAULT_SECONDS_CLAUDE,
        maxRunningTurns: ACCOUNT_RUNNING_TURNS_DEFAULT_CLAUDE,
      }
    : {
        burst: ACCOUNT_START_BURST_DEFAULT_CODEX,
        pauseSeconds: ACCOUNT_START_PAUSE_DEFAULT_SECONDS_CODEX,
        maxRunningTurns: ACCOUNT_RUNNING_TURNS_DEFAULT_CODEX,
      };
}

/** The one-line summary of the limits that apply: "8 per burst · 30 s pause · 16 running". */
export const throttleSummary = (throttle: Throttle) =>
  `${throttle.burst} per burst · ${throttle.pauseSeconds} s pause · ${throttle.maxRunningTurns} running`;

export default function AccountStartThrottle({
  account,
  onSaved,
}: {
  account: Account;
  onSaved: (state: PostResult<"/api/accounts/throttle">) => void;
}) {
  const { post } = useServerApi();
  const [values, setValues] = useState(() => accountThrottle(account));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [saved, setSaved] = useState(false);
  const lock = useRef(false);
  const save = async () => {
    if (lock.current) return;
    lock.current = true;
    setBusy(true);
    setError("");
    setSaved(false);
    try {
      onSaved(
        await post("/api/accounts/throttle", {
          account_key: account.id,
          ...values,
        }),
      );
      setSaved(true);
    } catch (failure) {
      setError(errorText(failure));
    } finally {
      lock.current = false;
      setBusy(false);
    }
  };
  return (
    <details className="account-register account-start-throttle">
      <summary>
        Turn start limits: {throttleSummary(accountThrottle(account))}
      </summary>
      <form
        onSubmit={(event) => {
          event.preventDefault();
          void save();
        }}
      >
        <Stack gap="sm" w="100%">
          {THROTTLE_FIELDS.map((spec) => (
            <NumberInput
              key={spec.field}
              label={spec.label}
              aria-label={`${spec.label} for ${account.email || account.label}`}
              title={spec.tooltip}
              min={spec.min}
              max={spec.max}
              allowDecimal={false}
              allowNegative={false}
              clampBehavior="strict"
              value={values[spec.field]}
              disabled={busy}
              onChange={(next) => {
                if (typeof next !== "number") return;
                setSaved(false);
                setValues((current) => ({ ...current, [spec.field]: next }));
              }}
            />
          ))}
          {error && (
            <p className="account-action-error" role="alert">
              {error}
            </p>
          )}
          {saved && <p role="status">Turn start limits saved.</p>}
          <Button type="submit" variant="filled" loading={busy}>
            Save turn start limits
          </Button>
        </Stack>
      </form>
    </details>
  );
}

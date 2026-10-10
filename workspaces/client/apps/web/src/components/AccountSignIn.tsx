import ErrorDescription from "./ErrorDescription";
import { Button } from "@mantine/core";
import { Check, Copy, ExternalLink, Plus, RefreshCw, X } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import {
  clearStableRequestId,
  errorText,
  post,
  save,
  saved,
  stableRequestId,
  type PostResult,
} from "../api";
import type { Account, useAccounts } from "./Accounts";
import { copyText } from "../clipboard/clipboard";

export type LoginReceipt = PostResult<"/api/accounts/login">;
const active = (status?: string) =>
  ["starting", "pending", "uncertain"].includes(status || "");
export function deviceCodeCountdownSeconds(
  expiresAt: unknown,
  now: number,
): number | null {
  if (
    typeof expiresAt !== "number" ||
    !Number.isFinite(expiresAt) ||
    expiresAt <= 0 ||
    !Number.isFinite(now)
  )
    return null;
  return Math.max(0, Math.ceil(expiresAt - now));
}

export default function AccountSignIn({
  state,
  targetAccount,
  emailHint,
  label,
  onConnected,
}: {
  state: ReturnType<typeof useAccounts>;
  targetAccount?: Account;
  emailHint?: string;
  label?: string;
  onConnected?: (account: Account) => void;
}) {
  const storageKey = `account-sign-in:${state.scope || "local"}${targetAccount ? `:${targetAccount.id}` : ""}`;
  const [requestId, setRequestId] = useState(() =>
    saved<string>(storageKey, ""),
  );
  const [busy, setBusy] = useState("");
  const [error, setError] = useState("");
  const [copied, setCopied] = useState(false);
  const [now, setNow] = useState(() => Date.now() / 1000);
  const [expectedEmail, setExpectedEmail] = useState(emailHint || "");
  const lock = useRef(false);
  const reported = useRef("");
  const receipts = (state.data.logins || []).filter(
    (r) => r.reauthAccountKey === targetAccount?.id,
  );
  const receipt =
    receipts.find((r) => r.requestId === requestId) ||
    [...receipts].reverse().find((r) => active(r.status));
  const account = state.data.accounts.find(
    (a) => a.id === receipt?.resolvedAccountKey,
  );
  useEffect(() => {
    if (
      account &&
      receipt &&
      ["ready", "duplicate"].includes(receipt.status) &&
      reported.current !== receipt.requestId
    ) {
      reported.current = receipt.requestId;
      onConnected?.(account);
    }
  }, [account, onConnected, receipt]);
  const remember = (id: string) => {
    save(storageKey, id);
    setRequestId(id);
  };
  const store = (result: LoginReceipt) => {
    remember(result.requestId);
    state.setData((old) => ({
      ...old,
      logins: [
        ...(old.logins || []).filter((r) => r.requestId !== result.requestId),
        result,
      ],
    }));
  };
  const run = async (name: string, action: () => Promise<void>) => {
    if (lock.current) return;
    lock.current = true;
    setBusy(name);
    setError("");
    try {
      await action();
    } catch (failure) {
      setError(errorText(failure));
    } finally {
      lock.current = false;
      setBusy("");
    }
  };
  const start = (emailOverride?: string) =>
    run("start", async () => {
      // Keep the identity when the HTTP reply is lost, including across reloads.
      const id =
        receipt?.status === "uncertain"
          ? receipt.requestId
          : !receipt && requestId
            ? requestId
            : crypto.randomUUID();
      remember(id);
      const mutationKey = `${storageKey}:${id}:start`;
      const result = await post(
        "/api/accounts/login",
        {
          login_id: id,
          ...(targetAccount ? { account_key: targetAccount.id } : {}),
          ...(!targetAccount && (emailOverride || expectedEmail)
            ? { email: emailOverride || expectedEmail }
            : {}),
          ...(!targetAccount && label ? { label } : {}),
        },
        { timeoutMs: 30000, requestId: stableRequestId(mutationKey) },
      );
      clearStableRequestId(mutationKey);
      store({ ...result, requestId: id });
      setCopied(false);
      await state.refresh();
    });
  const cancel = () =>
    run("cancel", async () => {
      if (!receipt) return;
      const mutationKey = `${storageKey}:${receipt.requestId}:cancel`;
      store(
        await post(
          "/api/accounts/login/cancel",
          { login_id: receipt.requestId },
          {
            timeoutMs: 15000,
            requestId: stableRequestId(mutationKey),
          },
        ),
      );
      clearStableRequestId(mutationKey);
      await state.refresh();
    });
  let url: string | null = null;
  try {
    const value = new URL(receipt?.verificationUrl || "");
    if (
      value.protocol === "https:" &&
      !value.username &&
      !value.password &&
      (value.hostname === "openai.com" ||
        value.hostname.endsWith(".openai.com"))
    )
      url = value.href;
  } catch {
    /* A pending native response may have no URL yet. */
  }
  const connected = ["ready", "duplicate"].includes(receipt?.status || "");
  useEffect(() => {
    if (
      !receipt ||
      deviceCodeCountdownSeconds(receipt?.expiresAt, Date.now() / 1000) ===
        null ||
      !active(receipt.status)
    )
      return;
    const timer = setInterval(() => setNow(Date.now() / 1000), 1000);
    return () => clearInterval(timer);
  }, [receipt?.expiresAt, receipt?.status]);
  const expirySeconds = deviceCodeCountdownSeconds(receipt?.expiresAt, now);
  return (
    <section
      className="account-add"
      aria-label={targetAccount ? "Restore account sign-in" : "Add an account"}
    >
      <div className="account-add-heading">
        <div>
          <strong>{targetAccount ? "Sign in again" : "Codex"}</strong>
          <p>
            {targetAccount
              ? `Use ${targetAccount.email || targetAccount.label}. Your chats keep this account.`
              : expectedEmail
                ? `Studio checks that you sign in as ${expectedEmail}.`
                : "Use a separate login and account limits."}
          </p>
        </div>
        <Button
          variant="filled"
          color="indigo"
          leftSection={<Plus size={15} />}
          loading={busy === "start"}
          disabled={
            !!busy || ["starting", "pending"].includes(receipt?.status || "")
          }
          onClick={() => void start()}
        >
          {targetAccount ? "Start sign-in" : "Sign in"}
        </Button>
      </div>
      {receipt && (
        <section className="account-login" aria-label="Account sign-in">
          {connected ? (
            <p role="status">
              <Check size={16} />{" "}
              {account?.disconnected
                ? "This account is saved but disconnected. Reconnect it in Accounts to use it for new chats."
                : targetAccount
                  ? "Sign-in restored. Send a new instruction to continue."
                  : receipt.status === "duplicate"
                    ? "This account is already connected."
                    : "Account connected."}{" "}
              {account?.email}
            </p>
          ) : receipt.status === "cancelled" ? (
            <p role="status">Sign-in cancelled.</p>
          ) : receipt.status === "error" ? (
            <ErrorDescription
              value={receipt.error || "Sign-in failed. Try again."}
            />
          ) : (
            <>
              {receipt.userCode && url ? (
                <>
                  <strong>Complete sign-in in your browser</strong>
                  <p>
                    {targetAccount
                      ? `Use ${targetAccount.email || targetAccount.label}, then enter this code.`
                      : "Use the new account, then enter this code."}
                  </p>
                  <div className="account-login-code">
                    <code>{receipt.userCode}</code>
                    <Button
                      size="compact-xs"
                      variant="subtle"
                      leftSection={<Copy size={13} />}
                      onClick={() =>
                        void run("copy", async () => {
                          await copyText(receipt.userCode!);
                          setCopied(true);
                        })
                      }
                    >
                      {copied ? "Copied" : "Copy code"}
                    </Button>
                  </div>
                  <Button
                    variant="filled"
                    color="indigo"
                    component="a"
                    c="white"
                    href={url}
                    target="_blank"
                    rel="noopener noreferrer"
                    size="compact-sm"
                    rightSection={<ExternalLink size={13} />}
                  >
                    Open sign-in page
                  </Button>
                  <small role="status">Waiting for sign-in…</small>
                  {expirySeconds !== null && (
                    <small role="timer" aria-label="Code expiry countdown">
                      {expirySeconds === 0
                        ? "Code expired"
                        : `Code expires in ${Math.floor(expirySeconds / 60)}:${String(expirySeconds % 60).padStart(2, "0")}`}
                    </small>
                  )}
                </>
              ) : (
                <p role="status">
                  {receipt.status === "starting"
                    ? "Starting sign-in…"
                    : "The sign-in response is unconfirmed."}
                </p>
              )}
              {Boolean(receipt.error) && (
                <ErrorDescription value={receipt.error} />
              )}
              <div className="account-login-actions">
                <Button
                  size="compact-xs"
                  variant="subtle"
                  leftSection={<RefreshCw size={13} />}
                  disabled={!!busy}
                  onClick={() =>
                    void run("check", async () => {
                      await state.refresh();
                    })
                  }
                >
                  Check status
                </Button>
                {receipt.loginId && (
                  <Button
                    size="compact-xs"
                    variant="subtle"
                    leftSection={<X size={13} />}
                    loading={busy === "cancel"}
                    disabled={!!busy}
                    onClick={() => void cancel()}
                  >
                    Cancel sign-in
                  </Button>
                )}
              </div>
            </>
          )}
        </section>
      )}
      {error && (
        <p className="account-action-error" role="alert">
          {error}
        </p>
      )}
      {!targetAccount &&
        receipt?.status === "error" &&
        receipt.error?.includes("different account") &&
        receipt.email && (
          <div className="account-wrong-identity" role="alert">
            <p>
              You signed in as {receipt.email}. This account expects{" "}
              {expectedEmail}. Studio did not save the sign-in.
            </p>
            <Button
              variant="default"
              disabled={!!busy}
              onClick={() => {
                setExpectedEmail(receipt.email!);
                void start(receipt.email!);
              }}
            >
              Keep {receipt.email}
            </Button>
          </div>
        )}
    </section>
  );
}

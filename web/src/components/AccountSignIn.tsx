import { Button } from "@mantine/core";
import { Check, Copy, ExternalLink, Plus, RefreshCw, X } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { api, errorText, save, saved } from "../api";
import type { useAccounts } from "./Accounts";

export interface LoginReceipt {
  requestId: string;
  accountKey: string;
  resolvedAccountKey?: string;
  status: string;
  loginId?: string;
  verificationUrl?: string;
  userCode?: string;
  error?: string;
  createdAt?: number;
}
const active = (status?: string) =>
  ["starting", "pending", "uncertain"].includes(status || "");

export default function AccountSignIn({
  state,
  opened,
}: {
  state: ReturnType<typeof useAccounts>;
  opened: boolean;
}) {
  const storageKey = `account-sign-in:${state.scope || "local"}`;
  const [requestId, setRequestId] = useState(() =>
    saved<string>(storageKey, ""),
  );
  const [busy, setBusy] = useState("");
  const [error, setError] = useState("");
  const [copied, setCopied] = useState(false);
  const lock = useRef(false);
  const receipts = state.data.logins || [];
  const receipt =
    receipts.find((r) => r.requestId === requestId) ||
    [...receipts].reverse().find((r) => active(r.status));
  const account = state.data.accounts.find(
    (a) => a.id === receipt?.resolvedAccountKey,
  );
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
  useEffect(() => {
    if (!opened || !active(receipt?.status)) return;
    let live = true;
    let timer: ReturnType<typeof setTimeout>;
    const poll = async () => {
      if (document.visibilityState !== "hidden" && navigator.onLine)
        await state.refresh();
      if (live) timer = setTimeout(poll, 2500);
    };
    timer = setTimeout(poll, 2500);
    return () => {
      live = false;
      clearTimeout(timer);
    };
  }, [opened, receipt?.requestId, receipt?.status, state.refresh]);
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
  const start = () =>
    run("start", async () => {
      // Keep the identity when the HTTP reply is lost, including across reloads.
      const id = !receipt && requestId ? requestId : crypto.randomUUID();
      remember(id);
      const result = await api<LoginReceipt>(
        "/api/accounts/login",
        { request_id: id },
        { timeoutMs: 30000 },
      );
      store({ ...result, requestId: id });
      setCopied(false);
      await state.refresh();
    });
  const cancel = () =>
    run("cancel", async () => {
      if (!receipt) return;
      store(
        await api<LoginReceipt>(
          "/api/accounts/login/cancel",
          { request_id: receipt.requestId },
          { timeoutMs: 15000 },
        ),
      );
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
  return (
    <section className="account-add" aria-label="Add an account">
      <div className="account-add-heading">
        <div>
          <strong>Add an account</strong>
          <p>Run another team with its own login and limits.</p>
        </div>
        <Button
          leftSection={<Plus size={15} />}
          loading={busy === "start"}
          disabled={
            !!busy || ["starting", "pending"].includes(receipt?.status || "")
          }
          onClick={() => void start()}
        >
          Sign in to another account
        </Button>
      </div>
      {receipt && (
        <section className="account-login" aria-label="Account sign-in">
          {connected ? (
            <p role="status">
              <Check size={16} />{" "}
              {account?.disconnected
                ? "This account is saved but disconnected. Reconnect it in Accounts to use it for new chats."
                : receipt.status === "duplicate"
                  ? "This account is already connected."
                  : "Account connected."}{" "}
              {account?.email}
            </p>
          ) : receipt.status === "cancelled" ? (
            <p role="status">Sign-in cancelled.</p>
          ) : receipt.status === "error" ? (
            <p role="alert">{receipt.error || "Sign-in failed. Try again."}</p>
          ) : (
            <>
              {receipt.userCode && url ? (
                <>
                  <strong>Complete sign-in in your browser</strong>
                  <p>Use the new account, then enter this code.</p>
                  <div className="account-login-code">
                    <code>{receipt.userCode}</code>
                    <Button
                      size="compact-xs"
                      variant="subtle"
                      leftSection={<Copy size={13} />}
                      onClick={() =>
                        void run("copy", async () => {
                          await navigator.clipboard.writeText(
                            receipt.userCode!,
                          );
                          setCopied(true);
                        })
                      }
                    >
                      {copied ? "Copied" : "Copy code"}
                    </Button>
                  </div>
                  <Button
                    component="a"
                    href={url}
                    target="_blank"
                    rel="noopener noreferrer"
                    size="compact-sm"
                    rightSection={<ExternalLink size={13} />}
                  >
                    Open sign-in page
                  </Button>
                  <small role="status">Waiting for sign-in…</small>
                </>
              ) : (
                <p role="status">
                  {receipt.status === "starting"
                    ? "Starting sign-in…"
                    : "The sign-in response is unconfirmed."}
                </p>
              )}
              {receipt.error && <p role="alert">{receipt.error}</p>}
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
    </section>
  );
}

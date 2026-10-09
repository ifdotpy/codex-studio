import { Button, Modal, TextInput } from "@mantine/core";
import { CircleCheck, ExternalLink } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { errorText, get, post, save, saved, type PostResult } from "../api";
import type { Account } from "./Accounts";
import { ProviderMark } from "./AccountTiles";
import { ActionButton } from "./ui/primitives";
import "./claude-sign-in.css";
import { watchResourceReads } from "./watchResourceReads";

type Receipt = PostResult<"/api/accounts/claude/login">;
type LoginAction = "start" | "code" | "cancel";
const active = (receipt: Receipt | null) =>
  !receipt || ["starting", "pending"].includes(receipt.status);
function authUrl(value?: string | null): string | null {
  try {
    const url = new URL(value || "");
    if (
      url.protocol === "https:" &&
      !url.username &&
      !url.password &&
      !url.port &&
      ["claude.ai", "claude.com"].includes(url.hostname) &&
      ["/oauth/authorize", "/cai/oauth/authorize"].includes(url.pathname)
    )
      return url.href;
  } catch {
    /* The login may not have a URL yet. */
  }
  return null;
}

export default function ClaudeSignIn({
  account,
  scope,
  onClose,
  onReady,
}: {
  account: Account;
  scope?: string;
  onClose: () => void;
  onReady: () => unknown;
}) {
  const storageKey = `claude-sign-in:${scope || "local"}:${account.id}`;
  const [requestId, setRequestId] = useState(() =>
    saved<string>(storageKey, ""),
  );
  const [receipt, setReceipt] = useState<Receipt | null>(null);
  const [code, setCode] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState("");
  const lock = useRef(false);
  const refreshed = useRef("");
  const ready = useRef(onReady);
  ready.current = onReady;
  const store = (result: Receipt, id: string) => {
    if (result.requestId !== id || result.accountKey !== account.id)
      throw new Error(
        "The sign-in response belongs to another account or request.",
      );
    setReceipt(result);
    setError("");
    if (result.status === "ready" && refreshed.current !== id) {
      refreshed.current = id;
      void Promise.resolve(ready.current()).catch((failure) =>
        setError(errorText(failure)),
      );
    }
  };
  useEffect(() => {
    if (!requestId || !active(receipt)) return;
    let live = true;
    const stop = watchResourceReads(
      { kind: "accounts" },
      async () => {
        const result = await get("/api/accounts/claude/login", {
          query: { request_id: requestId },
        });
        if (live) store(result, requestId);
      },
      (failure) => {
        if (live) setError(errorText(failure));
      },
    );
    return () => {
      live = false;
      stop();
    };
  }, [requestId, receipt?.status]);
  const run = async (action: LoginAction) => {
    if (lock.current) return;
    lock.current = true;
    setBusy(action);
    setError("");
    try {
      const id =
        action === "start" && (!requestId || !active(receipt))
          ? crypto.randomUUID()
          : requestId;
      if (action === "start") {
        save(storageKey, id);
        setRequestId(id);
        setReceipt(null);
      }
      const submittedCode = code.trim();
      if (action === "code") setCode("");
      const result =
        action === "start"
          ? await post(
              "/api/accounts/claude/login",
              { request_id: id, account_key: account.id },
              { timeoutMs: 30000 },
            )
          : action === "code"
            ? await post(
                "/api/accounts/claude/login/code",
                { request_id: id, code: submittedCode },
                { timeoutMs: 30000 },
              )
            : await post(
                "/api/accounts/claude/login/cancel",
                { request_id: id },
                { timeoutMs: 30000 },
              );
      store(result, id);
    } catch (failure) {
      setError(errorText(failure));
    } finally {
      lock.current = false;
      setBusy("");
    }
  };
  const url = authUrl(receipt?.verificationUrl);
  const pending = !!requestId && active(receipt);
  const signedIn = receipt?.status === "ready";
  // A lost start reply leaves a request without a receipt; let the user ask again.
  const canCheck = pending && !receipt;
  const problem = error || (receipt?.error ? errorText(receipt.error) : "");
  return (
    <Modal
      opened
      onClose={onClose}
      title="Sign in to Claude"
      centered
      classNames={{ body: "claude-sign-in" }}
    >
      <div className="claude-sign-in-account">
        <span className="claude-sign-in-mark">
          <ProviderMark provider="claude" />
        </span>
        <span className="claude-sign-in-identity">
          <strong>{account.email || account.label}</strong>
          <small>Claude subscription</small>
        </span>
      </div>
      {signedIn ? (
        <>
          <p className="claude-sign-in-done" role="status">
            <CircleCheck size={18} aria-hidden="true" />
            <span>
              Signed in{receipt.email ? ` as ${receipt.email}` : ""}. You can
              retry the message.
            </span>
          </p>
          <div className="claude-sign-in-footer">
            <ActionButton
              disabled={!!busy}
              loading={busy === "start"}
              onClick={() => void run("start")}
            >
              Start new sign-in
            </ActionButton>
            <ActionButton actionRole="primary" onClick={onClose}>
              Done
            </ActionButton>
          </div>
        </>
      ) : pending ? (
        <>
          <p className="claude-sign-in-status" role="status">
            {receipt?.status === "pending"
              ? "Complete sign-in in your browser."
              : "Waiting for the sign-in request."}
          </p>
          {url && (
            <Button
              variant="filled"
              color="indigo"
              component="a"
              href={url}
              target="_blank"
              rel="noopener noreferrer"
              rightSection={<ExternalLink size={15} aria-hidden="true" />}
              fullWidth
            >
              Open Claude sign-in
            </Button>
          )}
          {receipt?.verificationUrl && !url && (
            <p className="claude-sign-in-error" role="alert">
              Claude returned an unsupported sign-in URL.
            </p>
          )}
          {receipt?.status === "pending" && (
            <form
              className="claude-sign-in-code"
              onSubmit={(event) => {
                event.preventDefault();
                void run("code");
              }}
            >
              <TextInput
                label="Authorization code"
                description="Paste it here if Claude shows a code."
                placeholder="Code"
                type="password"
                autoComplete="off"
                value={code}
                onChange={(event) => setCode(event.currentTarget.value)}
                disabled={!!busy}
              />
              <ActionButton
                actionRole="secondary"
                type="submit"
                disabled={!code.trim() || !!busy}
                loading={busy === "code"}
              >
                Submit code
              </ActionButton>
            </form>
          )}
          <div className="claude-sign-in-footer">
            <ActionButton
              disabled={!!busy}
              onClick={() => void run("cancel")}
              loading={busy === "cancel"}
            >
              Cancel sign-in
            </ActionButton>
            {canCheck && (
              <ActionButton
                actionRole="secondary"
                disabled={!!busy}
                loading={busy === "start"}
                onClick={() => void run("start")}
              >
                Check sign-in request
              </ActionButton>
            )}
          </div>
        </>
      ) : (
        <>
          {receipt?.status === "cancelled" && (
            <p className="claude-sign-in-status" role="status">
              Sign-in cancelled.
            </p>
          )}
          <ActionButton
            actionRole="primary"
            fullWidth
            data-autofocus
            onClick={() => void run("start")}
            disabled={!!busy}
            loading={busy === "start"}
          >
            Start sign-in
          </ActionButton>
        </>
      )}
      {problem && (
        <p className="claude-sign-in-error" role="alert">
          {problem}
        </p>
      )}
    </Modal>
  );
}

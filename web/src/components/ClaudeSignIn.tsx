import { Button, Group, Modal, Stack, TextInput } from "@mantine/core";
import { useEffect, useRef, useState } from "react";
import {
  clearStableRequestId,
  errorText,
  get,
  post,
  save,
  saved,
  stableRequestId,
  type PostResult,
} from "../api";
import { copyText } from "../clipboard/clipboard";
import type { Account } from "./Accounts";
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
  serverLabel,
}: {
  account: Account;
  scope?: string;
  onClose: () => void;
  onReady: () => unknown;
  serverLabel?: string;
}) {
  const storageKey = `claude-sign-in:${scope || "local"}:${account.id}`;
  const [requestId, setRequestId] = useState(() =>
    saved<string>(storageKey, ""),
  );
  const [receipt, setReceipt] = useState<Receipt | null>(null);
  const [code, setCode] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState("");
  const [copied, setCopied] = useState(false);
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
      const mutationKey = `${storageKey}:${id}:${action}`;
      const requestOptions = {
        timeoutMs: 30000,
        requestId: stableRequestId(mutationKey),
      };
      const result =
        action === "start"
          ? await post(
              "/api/accounts/claude/login",
              { login_id: id, account_key: account.id },
              requestOptions,
            )
          : action === "code"
            ? await post(
                "/api/accounts/claude/login/code",
                { login_id: id, code: submittedCode },
                requestOptions,
              )
            : await post(
                "/api/accounts/claude/login/cancel",
                { login_id: id },
                requestOptions,
              );
      clearStableRequestId(mutationKey);
      if (action === "code") setCode("");
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
  return (
    <Modal
      opened
      onClose={onClose}
      title={`Sign in to Claude · ${account.label}${serverLabel ? ` · ${serverLabel}` : ""}`}
      centered
    >
      <Stack gap="sm">
        {serverLabel && <p>Server: {serverLabel}</p>}
        <p>
          Use your Claude subscription for{" "}
          <strong>{account.email || account.label}</strong>.
        </p>
        {receipt?.status === "ready" ? (
          <p role="status">
            Signed in{receipt.email ? ` as ${receipt.email}` : ""}. You can
            retry the message.
          </p>
        ) : (
          <>
            {pending && (
              <p role="status">
                {receipt?.status === "pending"
                  ? "Complete sign-in in your browser."
                  : "Waiting for the sign-in request."}
              </p>
            )}
            {url && (
              <Group>
                <Button
                  component="a"
                  href={url}
                  target="_blank"
                  rel="noopener noreferrer"
                >
                  Open Claude sign-in
                </Button>
                <Button
                  variant="default"
                  disabled={!!busy}
                  onClick={() =>
                    void (async () => {
                      try {
                        await copyText(url);
                        setCopied(true);
                      } catch (failure) {
                        setError(errorText(failure));
                      }
                    })()
                  }
                >
                  {copied ? "Link copied" : "Copy link"}
                </Button>
              </Group>
            )}
            {receipt?.verificationUrl && !url && (
              <p role="alert">Claude returned an unsupported sign-in URL.</p>
            )}
            {receipt?.status === "pending" && (
              <form
                onSubmit={(event) => {
                  event.preventDefault();
                  void run("code");
                }}
              >
                <Stack gap="sm">
                  <TextInput
                    label="Authorization code"
                    description="Paste the code if Claude asks you to return it here."
                    type="password"
                    autoComplete="off"
                    value={code}
                    onChange={(event) => setCode(event.currentTarget.value)}
                    disabled={!!busy}
                  />
                  <Button
                    type="submit"
                    disabled={!code.trim() || !!busy}
                    loading={busy === "code"}
                  >
                    Submit code
                  </Button>
                </Stack>
              </form>
            )}
            {receipt?.status === "cancelled" && (
              <p role="status">Sign-in cancelled.</p>
            )}
            <Group>
              <Button
                onClick={() => void run("start")}
                disabled={!!busy || (!!receipt && active(receipt))}
                loading={busy === "start"}
              >
                {pending ? "Check sign-in request" : "Start sign-in"}
              </Button>
              {pending && (
                <Button
                  variant="subtle"
                  disabled={!!busy}
                  onClick={() => void run("cancel")}
                  loading={busy === "cancel"}
                >
                  Cancel sign-in
                </Button>
              )}
            </Group>
          </>
        )}
        {Boolean(error || receipt?.error) && (
          <p role="alert">{error || errorText(receipt?.error)}</p>
        )}
        {receipt?.status === "ready" && (
          <Button disabled={!!busy} onClick={() => void run("start")}>
            Start new sign-in
          </Button>
        )}
        <Button variant="subtle" onClick={onClose}>
          Close
        </Button>
      </Stack>
    </Modal>
  );
}

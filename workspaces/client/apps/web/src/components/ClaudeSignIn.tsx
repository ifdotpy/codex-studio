import { Button, Group, Modal, Stack } from "@mantine/core";
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
import { ClaudeAuthorizationCodeForm } from "./ClaudeAuthorizationCodeForm";
import {
  codeSubmissionNotApplied,
  codeSubmissionResolved,
  codeVerificationPending,
  currentClaudeLoginReadMatches,
} from "./claudeCodeSubmission";
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
  const uncertaintyKey = `${storageKey}:code-submission-uncertain`;
  const [receipt, setReceipt] = useState<Receipt | null>(null);
  const [code, setCode] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState("");
  const [copied, setCopied] = useState(false);
  const [codeSubmissionUncertain, setCodeSubmissionUncertain] = useState(() =>
    saved<boolean>(uncertaintyKey, false),
  );
  const lock = useRef(false);
  const preflightRejected = useRef(false);
  const refreshed = useRef("");
  const currentRequestId = useRef(requestId);
  const statusReadGeneration = useRef(0);
  const refreshStatus = useRef<() => void>(() => {});
  const ready = useRef(onReady);
  ready.current = onReady;
  const setCodeSubmissionUnknown = (value: boolean) => {
    save(uncertaintyKey, value);
    setCodeSubmissionUncertain(value);
  };
  const store = (result: Receipt, id: string, fromStatusRead = false) => {
    if (result.requestId !== id || result.accountKey !== account.id)
      throw new Error(
        "The sign-in response belongs to another account or request.",
      );
    setReceipt(result);
    if (
      !fromStatusRead ||
      result.status !== "pending" ||
      result.codeSubmitted === true
    ) {
      preflightRejected.current = false;
      setError("");
    }
    if (codeSubmissionResolved(result)) setCodeSubmissionUnknown(false);
    if (result.status === "ready" && refreshed.current !== id) {
      refreshed.current = id;
      void Promise.resolve(ready.current()).catch((failure) =>
        setError(errorText(failure)),
      );
    }
  };
  useEffect(() => {
    if (!requestId || !active(receipt) || busy) return;
    let live = true;
    const readStatus = async () => {
      if (lock.current) return;
      const readGeneration = statusReadGeneration.current;
      const id = requestId;
      try {
        const result = await get("/api/accounts/claude/login", {
          query: { request_id: id },
        });
        if (
          !live ||
          lock.current ||
          !currentClaudeLoginReadMatches(
            currentRequestId.current,
            id,
            result.requestId,
            statusReadGeneration.current,
            readGeneration,
          )
        )
          return;
        store(result, id, true);
      } catch (failure) {
        if (
          live &&
          !lock.current &&
          currentRequestId.current === id &&
          statusReadGeneration.current === readGeneration
        )
          throw failure;
      }
    };
    const stop = watchResourceReads(
      { kind: "accounts" },
      readStatus,
      (failure) => {
        if (live) setError(errorText(failure));
      },
    );
    const refresh = stop.refresh;
    refreshStatus.current = refresh;
    refresh();
    return () => {
      live = false;
      stop();
      if (refreshStatus.current === refresh) refreshStatus.current = () => {};
    };
  }, [busy, requestId, receipt?.status]);
  const run = async (action: LoginAction) => {
    if (lock.current) return;
    statusReadGeneration.current += 1;
    lock.current = true;
    setBusy(action);
    setError("");
    try {
      const id =
        action === "start" && (!requestId || !active(receipt))
          ? crypto.randomUUID()
          : requestId;
      if (action === "start") {
        if (id !== requestId) {
          setCodeSubmissionUnknown(false);
          preflightRejected.current = false;
        }
        save(storageKey, id);
        setRequestId(id);
        currentRequestId.current = id;
        setReceipt(null);
      }
      const submittedCode = code.trim();
      if (action === "code") preflightRejected.current = false;
      if (action === "code") setCodeSubmissionUnknown(true);
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
      if (action === "code") {
        setCode("");
        setCodeSubmissionUnknown(!codeSubmissionResolved(result));
      }
      store(result, id);
    } catch (failure) {
      if (action === "code") {
        if (codeSubmissionNotApplied(failure)) {
          setCodeSubmissionUnknown(false);
          preflightRejected.current = true;
        } else {
          setCodeSubmissionUnknown(true);
          preflightRejected.current = false;
          setCode("");
        }
      }
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
                {codeSubmissionUncertain
                  ? "Studio could not confirm whether Claude received the authorization response. Check sign-in status or cancel this request; do not submit it again."
                  : codeVerificationPending(receipt)
                    ? "The full authorization response was submitted. Waiting for Claude to verify it; do not submit it again."
                    : receipt?.status === "pending"
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
            {receipt?.status === "pending" &&
              !codeVerificationPending(receipt) &&
              !codeSubmissionUncertain && (
                <ClaudeAuthorizationCodeForm
                  value={code}
                  onChange={(value) => {
                    setCode(value);
                    if (preflightRejected.current) {
                      preflightRejected.current = false;
                      setError("");
                    }
                  }}
                  onSubmit={() => void run("code")}
                  disabled={!!busy}
                  loading={busy === "code"}
                  submitLabel="Submit authorization response"
                />
              )}
            {receipt?.status === "cancelled" && (
              <p role="status">Sign-in cancelled.</p>
            )}
            <Group>
              {!requestId ? (
                <Button
                  onClick={() => void run("start")}
                  disabled={!!busy}
                  loading={busy === "start"}
                >
                  Start sign-in
                </Button>
              ) : pending ? (
                <Button
                  onClick={() => refreshStatus.current()}
                  disabled={!!busy}
                >
                  Check sign-in status
                </Button>
              ) : null}
              {requestId && !receipt && !codeSubmissionUncertain && (
                <Button
                  onClick={() => void run("start")}
                  disabled={!!busy}
                  loading={busy === "start"}
                >
                  Retry sign-in
                </Button>
              )}
              {pending && requestId && (
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
        {(receipt?.status === "error" || receipt?.status === "cancelled") && (
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

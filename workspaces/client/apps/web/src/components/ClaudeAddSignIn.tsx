import { Button, Group, Modal, Stack, TextInput } from "@mantine/core";
import { useEffect, useRef, useState } from "react";
import { errorText, get, post, save, saved, type PostResult } from "../api";
import { copyText } from "../clipboard/clipboard";
import { watchResourceReads } from "./watchResourceReads";

type Receipt = PostResult<"/api/accounts/claude/login">;
type SavedFlow = {
  requestId: string;
  startRequestId: string;
  codeRequestId?: string;
  cancelRequestId?: string;
  label: string;
  email: string;
  createdAt: number;
  unknownSince?: number;
};
const UNKNOWN_REQUEST_GRACE_MS = 5000;
const terminal = (status?: string) =>
  ["ready", "error", "cancelled"].includes(status || "");
const attemptKey = (key: string, id: string) => `${key}:${id}`;

function readSavedFlow(key: string): SavedFlow | null {
  const active = saved<unknown>(key, null);
  if (typeof active === "string") {
    const flow = saved<SavedFlow | null>(attemptKey(key, active), null);
    if (!flow) save(key, null);
    return flow;
  }
  if (
    active &&
    typeof active === "object" &&
    "requestId" in active &&
    typeof active.requestId === "string"
  ) {
    const legacy = active as SavedFlow;
    save(attemptKey(key, legacy.requestId), {
      ...legacy,
      createdAt: legacy.createdAt || Date.now(),
    });
    save(key, legacy.requestId);
    return { ...legacy, createdAt: legacy.createdAt || Date.now() };
  }
  return null;
}

function isUnknownRequest(failure: unknown) {
  const status =
    failure && typeof failure === "object" && "status" in failure
      ? failure.status
      : undefined;
  return (
    status === 404 || /unknown claude sign-in request/i.test(errorText(failure))
  );
}

function allowedUrl(value?: string | null) {
  try {
    const url = new URL(value || "");
    return url.protocol === "https:" &&
      !url.username &&
      !url.password &&
      !url.port &&
      url.hostname === "claude.com" &&
      url.pathname === "/cai/oauth/authorize"
      ? url.href
      : null;
  } catch {
    return null;
  }
}

export default function ClaudeAddSignIn({
  label,
  email,
  serverLabel,
  scope,
  opened,
  onClose,
  onReady,
}: {
  label: string;
  email: string;
  serverLabel: string;
  scope: string;
  opened: boolean;
  onClose: () => void;
  onReady: () => unknown;
}) {
  const storageKey = `claude-add-sign-in:${scope}`;
  const [flow, setFlow] = useState<SavedFlow | null>(() =>
    readSavedFlow(storageKey),
  );
  const requestId = flow?.requestId || "";
  const [receipt, setReceipt] = useState<Receipt | null>(null);
  const [expectedEmail, setExpectedEmail] = useState(email);
  const [code, setCode] = useState("");
  const [copied, setCopied] = useState(false);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState("");
  const [now, setNow] = useState(() => Date.now());
  const lock = useRef(false);
  const readyRequest = useRef("");
  const persistFlow = (next: SavedFlow) => {
    save(attemptKey(storageKey, next.requestId), next);
    save(storageKey, next.requestId);
    setFlow(next);
  };
  const clearSavedFlow = (savedFlow: SavedFlow) => {
    save(attemptKey(storageKey, savedFlow.requestId), null);
    if (saved<string>(storageKey, "") === savedFlow.requestId)
      save(storageKey, null);
  };
  const start = async (emailHint = expectedEmail, forceNew = false) => {
    if (lock.current) return;
    lock.current = true;
    setBusy("start");
    setError("");
    const canResume =
      !forceNew &&
      flow &&
      (!receipt || ["starting", "pending"].includes(receipt.status));
    const next: SavedFlow = canResume
      ? { ...flow, unknownSince: undefined }
      : {
          requestId: crypto.randomUUID(),
          startRequestId: crypto.randomUUID(),
          label: label.trim() || "Claude Code",
          email: emailHint.trim(),
          createdAt: Date.now(),
        };
    if (!canResume && flow) clearSavedFlow(flow);
    persistFlow(next);
    setReceipt(null);
    try {
      const result = await post(
        "/api/accounts/claude/add",
        {
          login_id: next.requestId,
          label: next.label,
          ...(next.email ? { email: next.email } : {}),
        },
        { requestId: next.startRequestId },
      );
      setReceipt(result);
      setExpectedEmail(next.email);
    } catch (failure) {
      setError(errorText(failure));
    } finally {
      lock.current = false;
      setBusy("");
    }
  };
  const run = async (action: "code" | "cancel") => {
    if (lock.current || !requestId || !flow) return;
    lock.current = true;
    setBusy(action);
    setError("");
    const submitted = code.trim();
    const requestField =
      action === "code" ? "codeRequestId" : "cancelRequestId";
    const mutationId = flow[requestField] || crypto.randomUUID();
    const nextFlow = { ...flow, [requestField]: mutationId };
    persistFlow(nextFlow);
    try {
      const result = await post(
        action === "code"
          ? "/api/accounts/claude/login/code"
          : "/api/accounts/claude/login/cancel",
        action === "code"
          ? { login_id: requestId, code: submitted }
          : { login_id: requestId },
        { requestId: mutationId },
      );
      setReceipt(result);
      if (action === "code") setCode("");
      persistFlow({ ...nextFlow, [requestField]: undefined });
    } catch (failure) {
      if (action === "cancel" && isUnknownRequest(failure)) {
        clearSavedFlow(flow);
        setFlow(null);
        setReceipt(null);
        setError("");
      } else setError(errorText(failure));
    } finally {
      lock.current = false;
      setBusy("");
    }
  };
  useEffect(() => {
    if (!requestId || terminal(receipt?.status) || busy === "start") return;
    let live = true;
    const poll = async () => {
      try {
        const result = await get("/api/accounts/claude/login", {
          query: { request_id: requestId },
        });
        if (!live || result.requestId !== requestId) return;
        setReceipt(result);
        if (result.status === "ready" && readyRequest.current !== requestId) {
          readyRequest.current = requestId;
          void Promise.resolve(onReady()).catch((failure) =>
            setError(errorText(failure)),
          );
        }
      } catch (failure) {
        if (!live) return;
        if (isUnknownRequest(failure) && flow && !flow.unknownSince) {
          persistFlow({ ...flow, unknownSince: Date.now() });
          setError("");
        } else if (!isUnknownRequest(failure)) setError(errorText(failure));
      }
    };
    const stop = watchResourceReads({ kind: "accounts" }, poll, (failure) => {
      if (live) setError(errorText(failure));
    });
    const timer = setInterval(() => void poll(), 1500);
    void poll();
    return () => {
      live = false;
      stop();
      clearInterval(timer);
    };
  }, [busy, flow?.unknownSince, onReady, receipt?.status, requestId]);
  useEffect(() => {
    if (!flow?.unknownSince || !requestId || receipt) return;
    const remaining =
      UNKNOWN_REQUEST_GRACE_MS - (Date.now() - flow.unknownSince);
    if (remaining <= 0) {
      setNow(Date.now());
      return;
    }
    const timer = setTimeout(() => setNow(Date.now()), remaining);
    return () => clearTimeout(timer);
  }, [flow?.unknownSince, receipt, requestId]);
  useEffect(() => {
    if (flow && terminal(receipt?.status)) clearSavedFlow(flow);
  }, [flow?.requestId, receipt?.status]);
  const unknownTooLong =
    !!flow?.unknownSince &&
    !receipt &&
    now - flow.unknownSince >= UNKNOWN_REQUEST_GRACE_MS;
  const close = () => {
    if (flow && terminal(receipt?.status)) clearSavedFlow(flow);
    onClose();
  };
  const url = allowedUrl(receipt?.verificationUrl);
  const displayLabel = flow?.label || label;
  const displayEmail = flow?.email || expectedEmail || email;
  const wrong = !!receipt?.error?.includes("different Claude account");
  const expired = !!receipt?.error?.includes("expired");
  const invalid = !!receipt?.error?.includes("rejected");
  const shownError = wrong
    ? `You signed in as ${receipt?.email || "another account"}. This account expects ${email || "a different account"}. Studio did not save the sign-in.`
    : expired
      ? "The sign-in expired. Start a new sign-in to get a new link."
      : invalid
        ? "The code is not valid. Copy the full code from the Claude page, then paste it again."
        : error || receipt?.error || "";
  return (
    <Modal
      opened={opened}
      onClose={close}
      title={`Sign in to Claude · ${displayLabel}`}
      centered
    >
      <Stack gap="sm">
        <TextInput label="Server" value={serverLabel} readOnly />
        {displayEmail && <p>Sign in as {displayEmail}.</p>}
        {receipt?.status === "ready" ? (
          <p role="status">
            Signed in as {receipt.email || email || "Claude account"}.
          </p>
        ) : (
          <>
            {receipt?.status === "cancelled" && (
              <p role="status">Sign-in cancelled.</p>
            )}
            {url && (
              <Group>
                <Button
                  component="a"
                  href={url}
                  target="_blank"
                  rel="noopener noreferrer"
                >
                  Open sign-in page
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
              <p role="alert">Claude returned an unsupported sign-in link.</p>
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
                    label="Paste code"
                    description="Paste the full code, including #state."
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
                    Finish sign-in
                  </Button>
                </Stack>
              </form>
            )}
            {!requestId || terminal(receipt?.status) ? (
              <Button
                onClick={() => void start(displayEmail)}
                loading={busy === "start"}
                disabled={!!busy}
              >
                {receipt ? "Start again" : "Start sign-in"}
              </Button>
            ) : (
              !receipt && (
                <Group>
                  <Button
                    onClick={() => void start(displayEmail)}
                    loading={busy === "start"}
                    disabled={!!busy}
                  >
                    Retry sign-in
                  </Button>
                  {unknownTooLong && (
                    <Button
                      variant="default"
                      onClick={() => void start(displayEmail, true)}
                      disabled={!!busy}
                    >
                      Start again
                    </Button>
                  )}
                </Group>
              )
            )}
            {requestId &&
              receipt &&
              ["starting", "pending"].includes(receipt.status) && (
                <p role="status">Waiting for sign-in on {serverLabel}…</p>
              )}
            {requestId &&
              (["starting", "pending"].includes(receipt?.status || "") ||
                !receipt) && (
                <Button
                  variant="subtle"
                  onClick={() => void run("cancel")}
                  disabled={!!busy}
                  loading={busy === "cancel"}
                >
                  Cancel
                </Button>
              )}
            {unknownTooLong && (
              <p role="status">
                The server has not found this request. Retry it, start again, or
                cancel.
              </p>
            )}
            {wrong && receipt?.email && (
              <Button
                variant="default"
                onClick={() => {
                  setExpectedEmail(receipt.email!);
                  void start(receipt.email!, true);
                }}
                disabled={!!busy}
              >
                Keep {receipt.email}
              </Button>
            )}
          </>
        )}
        {shownError && <p role="alert">{shownError}</p>}
        <Group justify="flex-end">
          <Button variant="subtle" onClick={close}>
            Close
          </Button>
        </Group>
      </Stack>
    </Modal>
  );
}

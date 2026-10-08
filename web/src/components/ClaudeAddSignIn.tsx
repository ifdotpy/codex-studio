import { Button, Group, Modal, Stack, TextInput } from "@mantine/core";
import { useEffect, useRef, useState } from "react";
import { errorText, get, post, type PostResult } from "../api";
import { watchResourceReads } from "./watchResourceReads";

type Receipt = PostResult<"/api/accounts/claude/login">;

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
  onClose,
  onReady,
}: {
  label: string;
  email: string;
  serverLabel: string;
  onClose: () => void;
  onReady: () => unknown;
}) {
  const [requestId, setRequestId] = useState("");
  const [receipt, setReceipt] = useState<Receipt | null>(null);
  const [expectedEmail, setExpectedEmail] = useState(email);
  const [code, setCode] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState("");
  const lock = useRef(false);
  const readyRequest = useRef("");
  const start = async (emailHint = expectedEmail) => {
    if (lock.current) return;
    lock.current = true;
    setBusy("start");
    setError("");
    const id = crypto.randomUUID();
    setRequestId(id);
    setReceipt(null);
    try {
      const result = await post("/api/accounts/claude/add", {
        request_id: id,
        label: label.trim() || "Claude Code",
        ...(emailHint.trim() ? { email: emailHint.trim() } : {}),
      });
      setReceipt(result);
    } catch (failure) {
      setError(errorText(failure));
    } finally {
      lock.current = false;
      setBusy("");
    }
  };
  const run = async (action: "code" | "cancel") => {
    if (lock.current || !requestId) return;
    lock.current = true;
    setBusy(action);
    setError("");
    const submitted = code.trim();
    if (action === "code") setCode("");
    try {
      const result = await post(
        action === "code"
          ? "/api/accounts/claude/login/code"
          : "/api/accounts/claude/login/cancel",
        action === "code"
          ? { request_id: requestId, code: submitted }
          : { request_id: requestId },
      );
      setReceipt(result);
    } catch (failure) {
      setError(errorText(failure));
    } finally {
      lock.current = false;
      setBusy("");
    }
  };
  useEffect(() => {
    if (
      !requestId ||
      receipt?.status === "ready" ||
      receipt?.status === "error" ||
      receipt?.status === "cancelled"
    )
      return;
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
        if (live) setError(errorText(failure));
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
  }, [onReady, receipt?.status, requestId]);
  const url = allowedUrl(receipt?.verificationUrl);
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
      opened
      onClose={onClose}
      title={`Sign in to Claude · ${label}`}
      centered
    >
      <Stack gap="sm">
        <TextInput label="Server" value={serverLabel} readOnly />
        {expectedEmail && <p>Sign in as {expectedEmail}.</p>}
        {receipt?.status === "ready" ? (
          <p role="status">
            Signed in as {receipt.email || email || "Claude account"}.
          </p>
        ) : (
          <>
            {url && (
              <Button
                component="a"
                href={url}
                target="_blank"
                rel="noopener noreferrer"
              >
                Open sign-in page
              </Button>
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
            {!requestId ||
            receipt?.status === "error" ||
            receipt?.status === "cancelled" ? (
              <Button
                onClick={() => void start()}
                loading={busy === "start"}
                disabled={!!busy}
              >
                {receipt ? "Start again" : "Start sign-in"}
              </Button>
            ) : (
              <p role="status">Waiting for sign-in on {serverLabel}…</p>
            )}
            {requestId &&
              ["starting", "pending"].includes(receipt?.status || "") && (
                <Button
                  variant="subtle"
                  onClick={() => void run("cancel")}
                  disabled={!!busy}
                  loading={busy === "cancel"}
                >
                  Cancel
                </Button>
              )}
            {wrong && receipt?.email && (
              <Button
                variant="default"
                onClick={() => {
                  setExpectedEmail(receipt.email!);
                  void start(receipt.email!);
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
          <Button variant="subtle" onClick={onClose}>
            Close
          </Button>
        </Group>
      </Stack>
    </Modal>
  );
}

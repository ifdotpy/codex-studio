import { useEffect, useRef, useState } from "react";
import { api, ApiError } from "../api";
import { onResume } from "../sync/resume";

export interface ProgressLayout {
  revision: string;
  width: number;
  height: number;
  contentWidth: number;
  contentHeight: number;
  fits: boolean;
  reason: null | "overflow" | "unsupported";
}
const client = crypto.randomUUID();
let sequence = 0;

export function useProgressLayoutReport(
  agent: string,
  scope: string,
  layout: ProgressLayout | null,
) {
  const latest = useRef(layout);
  latest.current = layout;
  const wake = useRef<(() => void) | undefined>(undefined);
  const [failure, setFailure] = useState<{
    scope: string;
    revision: string;
    error: unknown;
  } | null>(null);
  useEffect(() => {
    let active = true;
    let request: AbortController | undefined;
    let requestKey = "";
    let timer: ReturnType<typeof setTimeout> | undefined;
    let pending = false;
    let sent = "";
    let sentAt = 0;
    let failures = 0;
    let retryAt = 0;
    let retryKey = "";
    const visible = () => !document.hidden && navigator.onLine !== false;
    const send = async () => {
      clearTimeout(timer);
      const candidate = latest.current;
      if (!active || !visible()) return;
      if (!candidate) {
        request?.abort();
        return;
      }
      const fingerprint = JSON.stringify(candidate);
      if (request) {
        pending = true;
        if (fingerprint !== requestKey) request.abort();
        return;
      }
      if (fingerprint === retryKey && retryAt > Date.now()) {
        timer = setTimeout(() => void send(), retryAt - Date.now());
        return;
      }
      const remaining = 30000 - (Date.now() - sentAt);
      if (fingerprint === sent && remaining > 0) {
        timer = setTimeout(() => void send(), remaining);
        return;
      }
      const current = new AbortController();
      request = current;
      requestKey = fingerprint;
      try {
        await api(
          "/api/panel/layout",
          {
            agent,
            client,
            sequence: ++sequence,
            renderer: "progress-markdown-v1",
            ...candidate,
          },
          { timeoutMs: 8000, signal: current.signal },
        );
        if (!active || current.signal.aborted) return;
        sent = fingerprint;
        sentAt = Date.now();
        failures = 0;
        retryAt = 0;
        retryKey = "";
        setFailure(null);
      } catch (error) {
        if (!active || current.signal.aborted || !visible()) return;
        // The next read/measurement replaces a stale revision or sequence.
        if (
          !(error instanceof ApiError && error.status === 409) &&
          latest.current?.revision === candidate.revision
        )
          setFailure({ scope, revision: candidate.revision, error });
        failures = Math.min(failures + 1, 5);
        retryKey = fingerprint;
        retryAt = Date.now() + Math.min(30000, 2000 * 2 ** (failures - 1));
      } finally {
        if (request === current) request = undefined;
        if (active && visible() && latest.current)
          timer = setTimeout(
            () => void send(),
            pending ? 0 : retryAt ? Math.max(0, retryAt - Date.now()) : 30000,
          );
        pending = false;
      }
    };
    const refresh = () => {
      void send();
    };
    wake.current = refresh;
    const stopResume = onResume(() => {
      retryAt = 0;
      refresh();
    });
    const pause = () => {
      if (visible()) return;
      clearTimeout(timer);
      request?.abort();
    };
    document.addEventListener("visibilitychange", pause);
    window.addEventListener("offline", pause);
    refresh();
    return () => {
      active = false;
      clearTimeout(timer);
      request?.abort();
      stopResume();
      document.removeEventListener("visibilitychange", pause);
      window.removeEventListener("offline", pause);
      if (wake.current === refresh) wake.current = undefined;
    };
  }, [agent, scope]);
  useEffect(() => {
    wake.current?.();
  }, [layout]);
  return failure?.scope === scope && failure.revision === layout?.revision
    ? failure.error
    : null;
}

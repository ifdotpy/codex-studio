import { useEffect, useRef, useState } from "react";
import { post, ApiError } from "../../api";
import { onResume } from "../../sync/resume";
import type { components } from "../../generated/api";
import {
  layoutReportFor,
  type LayoutMeasurement,
} from "./progressLayoutReport";

export type ProgressLayout = Pick<
  components["schemas"]["PanelLayoutBody"],
  | "revision"
  | "width"
  | "height"
  | "contentWidth"
  | "contentHeight"
  | "overflowX"
  | "overflowY"
  | "totalLines"
  | "visibleLines"
  | "lastVisibleLine"
  | "lastVisibleHeading"
  | "fits"
  | "reason"
>;
const client = crypto.randomUUID();
let sequence = 0;
const maxAutomaticRetries = 5;
const maxRetryDelayMs = 30000;

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
    let failures = 0;
    let retryAt = 0;
    let retryKey = "";
    let exhaustedKey = "";
    let measurement: LayoutMeasurement | undefined;
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
      if (exhaustedKey && fingerprint !== exhaustedKey) exhaustedKey = "";
      if (fingerprint === sent || fingerprint === exhaustedKey) return;
      if (request) {
        pending = true;
        if (fingerprint !== requestKey) request.abort();
        return;
      }
      if (fingerprint === retryKey && retryAt > Date.now()) {
        timer = setTimeout(() => void send(), retryAt - Date.now());
        return;
      }
      const current = new AbortController();
      request = current;
      requestKey = fingerprint;
      const nextMeasurement = layoutReportFor(
        measurement,
        agent,
        client,
        candidate,
        sequence + 1,
      );
      if (nextMeasurement !== measurement) {
        sequence += 1;
        failures = 0;
        retryAt = 0;
        retryKey = "";
      }
      measurement = nextMeasurement;
      try {
        await post("/api/panel/layout", nextMeasurement.body, {
          timeoutMs: 8000,
          signal: current.signal,
        });
        if (!active || current.signal.aborted) return;
        sent = fingerprint;
        failures = 0;
        retryAt = 0;
        retryKey = "";
        exhaustedKey = "";
        setFailure(null);
      } catch (error) {
        if (!active || current.signal.aborted || !visible()) return;
        // The next read/measurement replaces a stale revision or sequence.
        if (
          !(error instanceof ApiError && error.status === 409) &&
          latest.current?.revision === candidate.revision
        )
          setFailure({ scope, revision: candidate.revision, error });
        failures += 1;
        retryKey = fingerprint;
        if (failures > maxAutomaticRetries) {
          retryAt = 0;
          exhaustedKey = fingerprint;
        } else {
          retryAt =
            Date.now() + Math.min(maxRetryDelayMs, 2000 * 2 ** (failures - 1));
        }
      } finally {
        if (request === current) request = undefined;
        if (active && visible() && latest.current && pending)
          timer = setTimeout(() => void send(), 0);
        else if (
          active &&
          visible() &&
          retryAt &&
          failures <= maxAutomaticRetries
        )
          timer = setTimeout(
            () => void send(),
            Math.max(0, retryAt - Date.now()),
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
      failures = 0;
      exhaustedKey = "";
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

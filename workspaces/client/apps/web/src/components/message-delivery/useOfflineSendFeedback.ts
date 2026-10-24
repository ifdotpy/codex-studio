import { useEffect, useState } from "react";
import { useNetwork } from "@mantine/hooks";
import type { OutgoingMessage } from "../../sync/send";

// These browser diagnostics identify presentation duplicates, never retry policy.
export function connectionFailureText(value: string) {
  return /^(Failed to fetch|Load failed|Network request failed|NetworkError\b|The device is offline\b)/i.test(
    value.trim(),
  );
}

/** A network event does not confirm delivery; the existing outbox receipt does. */
export function useOfflineSendFeedback(
  entries: OutgoingMessage[],
  room: string | null,
  errors: string[],
) {
  const { online } = useNetwork();
  const [tracked, setTracked] = useState<string[]>([]);
  const [details, setDetails] = useState<string[]>([]);
  const currentErrors = [
    ...new Set(
      [...errors, ...entries.map((entry) => entry.error || "")].filter(
        connectionFailureText,
      ),
    ),
  ];
  const errorKey = currentErrors.join("\n");
  const disconnected = !online || !!errorKey;
  useEffect(() => {
    setTracked((previous) => {
      const next = entries
        .filter(
          (entry) =>
            entry.status === "queued" &&
            (disconnected || previous.includes(entry.id)),
        )
        .map((entry) => entry.id);
      return JSON.stringify(previous) === JSON.stringify(next)
        ? previous
        : next;
    });
  }, [entries, disconnected]);
  const pending = entries.some(
    (entry) =>
      entry.body.room === room &&
      entry.status === "queued" &&
      (disconnected || tracked.includes(entry.id)),
  );
  useEffect(() => {
    // Keep consumed diagnostics until their producers clear them. Otherwise an
    // old toast/read error would reappear just after delivery was confirmed.
    setDetails((previous) => {
      const next = pending
        ? [...new Set([...previous, ...errorKey.split("\n").filter(Boolean)])]
        : errorKey
          ? previous
          : [];
      return JSON.stringify(previous) === JSON.stringify(next)
        ? previous
        : next;
    });
  }, [pending, errorKey]);
  return {
    pending,
    details: [...new Set([...details, ...(pending ? currentErrors : [])])].join(
      "\n",
    ),
    owns: (error: string) =>
      connectionFailureText(error) && (pending || details.includes(error)),
  };
}

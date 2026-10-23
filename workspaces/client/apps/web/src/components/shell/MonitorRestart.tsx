import { Button, Text, Textarea, TextInput } from "@mantine/core";
import { useState } from "react";
import { ApiError, errorText, type PostBody } from "../../api";
import {
  useServerApi,
  useServerServices,
} from "../../servers/ServerServicesContext";
import type { DisplayBackgroundTask } from "../backgroundTaskModel";

type Restart = PostBody<"/api/monitor/restart">;
export function restartPatterns(text: string): string[] {
  return text.split("\n").filter((line) => line.length > 0);
}

export default function MonitorRestart({
  task,
  notify,
  refresh,
}: {
  task: DisplayBackgroundTask;
  notify: (message: string) => void;
  refresh: () => Promise<void>;
}) {
  const { post } = useServerApi();
  const services = useServerServices();
  const storageKey = `monitor-restart:${services?.owner.id || "local"}:${task.id}`;
  const [receipt, setReceipt] = useState<Restart | null>(() => {
    try {
      return JSON.parse(
        sessionStorage.getItem(storageKey) || "null",
      ) as Restart | null;
    } catch {
      return null;
    }
  });
  const [opened, setOpened] = useState(!!receipt);
  const [patterns, setPatterns] = useState(
    (task.outputPatterns || []).join("\n"),
  );
  const [pending, setPending] = useState(false);
  const [error, setError] = useState("");
  const values = restartPatterns(patterns);
  const unchanged =
    patterns === (task.outputPatterns || []).join("\n") ||
    JSON.stringify(values) === JSON.stringify(task.outputPatterns || []);
  const submit = async () => {
    if (pending || task.epoch == null) return;
    const body = receipt || {
      id: task.id,
      agent: task.agent,
      epoch: task.epoch,
      accountKey: task.accountKey || "default",
      requestId: crypto.randomUUID(),
      outputPatterns: values,
    };
    // Retain the exact confirmed request across a lost reply or drawer close.
    sessionStorage.setItem(storageKey, JSON.stringify(body));
    setReceipt(body);
    setPending(true);
    setError("");
    try {
      await post("/api/monitor/restart", body);
      sessionStorage.removeItem(storageKey);
      setReceipt(null);
      setOpened(false);
      notify(
        "Restart queued. The new command waits for the previous command to exit.",
      );
      void refresh().catch((failure) => notify(errorText(failure)));
    } catch (failure) {
      if (
        failure instanceof ApiError &&
        failure.status >= 400 &&
        failure.status < 500
      ) {
        sessionStorage.removeItem(storageKey);
        setReceipt(null);
        setError(errorText(failure));
      } else {
        setError(
          "The restart result is unknown. Check the same request before another restart.",
        );
      }
    } finally {
      setPending(false);
    }
  };
  if (!opened && task.cancelRequested) return null;
  if (!opened)
    return (
      <Button
        size="xs"
        variant="light"
        mt="xs"
        disabled={task.epoch == null}
        onClick={() => setOpened(true)}
      >
        Restart with narrower patterns
      </Button>
    );
  return (
    <div className="task-restart">
      <Text fw={600} size="sm">
        Restart with narrower patterns
      </Text>
      <TextInput label="Command" value={task.command || ""} readOnly />
      <Textarea
        label="Output patterns, one per line"
        value={receipt ? receipt.outputPatterns.join("\n") : patterns}
        disabled={pending || !!receipt}
        onChange={(event) => setPatterns(event.currentTarget.value)}
        autosize
        minRows={2}
      />
      <Text size="sm">
        Use more specific patterns. Confirm to stop the old command and run this
        command again after it exits.
      </Text>
      {error && (
        <Text role="alert" c="red" size="sm">
          {error}
        </Text>
      )}
      <div className="task-restart-actions">
        <Button
          size="xs"
          loading={pending}
          disabled={!receipt && (!values.length || unchanged)}
          onClick={() => void submit()}
        >
          {receipt ? "Check restart result" : "Confirm restart"}
        </Button>
        <Button
          size="xs"
          variant="subtle"
          disabled={pending || !!receipt}
          onClick={() => setOpened(false)}
        >
          Cancel
        </Button>
      </div>
    </div>
  );
}

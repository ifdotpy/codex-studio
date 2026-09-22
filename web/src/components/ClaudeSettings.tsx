import { useEffect, useState } from "react";
import { Button, NativeSelect, Switch, TextInput } from "@mantine/core";
import { api, errorText, saved } from "../api";
import { busy, type Agent, type Json } from "../types";

export function ClaudeSettings({
  agent,
  refresh,
}: {
  agent: Agent;
  refresh: () => Promise<unknown>;
}) {
  const [state, setState] = useState<Json>({}),
    [commands, setCommands] = useState<Json[]>([]);
  const [mode, setMode] = useState("default"),
    [thinking, setThinking] = useState(true);
  const [window, setWindow] = useState(""),
    [command, setCommand] = useState(""),
    [turn, setTurn] = useState("");
  const [error, setError] = useState(""),
    [pending, setPending] = useState(false);
  const call = (action: string, data: Json = {}) =>
    api("/api/claude/session", { id: agent.id, action, ...data });
  const mutate = async (action: string, data: Json) => {
    const storageKey = `claude-control:${agent.accountKey}:${agent.id}:${action}:${JSON.stringify(data)}`;
    const requestId =
      saved<string | null>(storageKey, null) || crypto.randomUUID();
    // Keep the same identity across a lost HTTP response or a renderer reload.
    localStorage.setItem(storageKey, JSON.stringify(requestId));
    const result = await call(action, { ...data, request_id: requestId });
    localStorage.removeItem(storageKey);
    return result;
  };
  const load = async () => {
    const value = await call("state");
    setState(value);
    setMode(
      value.settings?.permissionMode ||
        (agent.yoloMode === false ? "default" : "bypassPermissions"),
    );
    setThinking(value.settings?.thinking !== false);
    setWindow(String(value.settings?.autoCompactWindow || ""));
  };
  useEffect(() => {
    let cancelled = false;
    call("state")
      .then((value) => {
        if (cancelled) return;
        setState(value);
        setMode(
          value.settings?.permissionMode ||
            (agent.yoloMode === false ? "default" : "bypassPermissions"),
        );
        setThinking(value.settings?.thinking !== false);
        setWindow(String(value.settings?.autoCompactWindow || ""));
      })
      .catch((e) => !cancelled && setError(errorText(e)));
    return () => {
      cancelled = true;
    };
  }, [agent.id]);
  const run = async (work: () => Promise<unknown>) => {
    setPending(true);
    setError("");
    try {
      await work();
      await load();
      await refresh();
    } catch (e) {
      setError(errorText(e));
      try {
        await load();
      } catch {
        /* Keep the original action error. */
      }
    } finally {
      setPending(false);
    }
  };
  const historyTurns: Json[] = [...(state.turns || [])];
  const recoveryTurn = state.controlOperation?.turnId;
  if (recoveryTurn && !historyTurns.some((t) => t.id === recoveryTurn))
    historyTurns.push({ id: recoveryTurn, text: "Recover pending rollback" });
  useEffect(() => {
    if (turn && !historyTurns.some((t) => t.id === turn)) setTurn("");
  }, [state, turn]);
  const locked = busy.has(agent.status) || pending;
  return (
    <section className="settings-group" aria-label="Claude settings">
      <h2>Claude Code{state.version ? ` ${state.version}` : ""}</h2>
      <NativeSelect
        label="Permission mode"
        value={mode}
        disabled={locked}
        onChange={(e) => setMode(e.currentTarget.value)}
        data={[
          { value: "default", label: "Ask for permission" },
          { value: "acceptEdits", label: "Allow file edits" },
          { value: "auto", label: "Automatic" },
          { value: "plan", label: "Plan only" },
          { value: "bypassPermissions", label: "Full access" },
        ]}
      />
      <Switch
        label="Extended thinking"
        checked={thinking}
        disabled={locked}
        onChange={(e) => setThinking(e.currentTarget.checked)}
      />
      <TextInput
        label="Auto-compact token limit"
        placeholder="Use the Claude default"
        value={window}
        disabled={locked}
        onChange={(e) => setWindow(e.currentTarget.value)}
      />
      <Button
        disabled={locked}
        onClick={() =>
          run(() =>
            mutate("settings", {
              settings: {
                permissionMode: mode,
                thinking,
                ...(window ? { autoCompactWindow: Number(window) } : {}),
              },
            }),
          )
        }
      >
        Save Claude settings
      </Button>
      <details>
        <summary>Commands and skills</summary>
        <Button
          disabled={pending}
          onClick={() => run(async () => setCommands(await call("commands")))}
        >
          Load commands
        </Button>
        <NativeSelect
          label="Command"
          value={command}
          onChange={(e) => setCommand(e.currentTarget.value)}
          data={[
            { value: "", label: "Select a command" },
            ...commands.map((c) => ({
              value: "/" + c.name,
              label: "/" + c.name + " " + (c.description || ""),
            })),
          ]}
        />
        <TextInput
          label="Command and arguments"
          value={command}
          onChange={(e) => setCommand(e.currentTarget.value)}
        />
        <Button
          disabled={pending || !command}
          onClick={() => run(() => mutate("command", { command }))}
        >
          Send command
        </Button>
        <Button
          disabled={locked}
          onClick={() => run(() => mutate("command", { command: "/compact" }))}
        >
          Compact conversation
        </Button>
      </details>
      <details>
        <summary>Conversation history</summary>
        <p>
          Rollback removes the selected turn and later turns from Claude’s
          context. Saved history remains available.
        </p>
        <NativeSelect
          label="First turn to remove"
          value={turn}
          onChange={(e) => setTurn(e.currentTarget.value)}
          data={[
            { value: "", label: "Select a turn" },
            ...historyTurns.map((t: Json, i: number) => ({
              value: t.id,
              label: `${i + 1}. ${t.text || t.status}`,
            })),
          ]}
        />
        <Button
          color="red"
          disabled={locked || !turn || !historyTurns.some((t) => t.id === turn)}
          onClick={() =>
            run(() =>
              state.controlOperation?.turnId === turn
                ? call("rollback", {
                    turn_id: turn,
                    request_id: state.controlOperation.requestId,
                  })
                : mutate("rollback", { turn_id: turn }),
            )
          }
        >
          Roll back context
        </Button>
      </details>
      {(state.tasks || []).map((t: Json) => (
        <div key={t.task_id}>
          {t.description || t.task_id}
          <Button
            disabled={pending}
            onClick={() => run(() => call("stop_task", { task_id: t.task_id }))}
          >
            Stop task
          </Button>
        </div>
      ))}
      {error && <p role="alert">{error}</p>}
    </section>
  );
}

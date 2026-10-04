import { useEffect, useRef, useState } from "react";
import { Button, NativeSelect, Switch, TextInput } from "@mantine/core";
import { errorText, post, saved, type PostBody, type PostResult } from "../api";
import { busy, type Agent } from "../types";
import { useWorkerModels } from "./agents/WorkerModelPicker";
import AccountSignInNotice from "./AccountSignInNotice";
import type { Account } from "./Accounts";
import { requiresThinking } from "../../../scripts/claude_bridge/thinking.mjs";
import "./claude-settings.css";

type ClaudeValues = {
  permissionMode: NonNullable<
    NonNullable<ClaudeSessionState["settings"]>["permissionMode"]
  >;
  thinking: boolean;
  autoCompactWindow?: number;
};

function permissionMode(value: string): ClaudeValues["permissionMode"] | null {
  switch (value) {
    case "default":
    case "acceptEdits":
    case "auto":
    case "plan":
    case "bypassPermissions":
      return value;
    default:
      return null;
  }
}

type ClaudeSessionRequest = PostBody<"/api/claude/session">;
type ClaudeSessionResponse = PostResult<"/api/claude/session">;
type ClaudeSessionState = Extract<ClaudeSessionResponse, { turns: unknown[] }>;
type ClaudeCommandsResponse = Extract<ClaudeSessionResponse, unknown[]>;
type ClaudeSessionMutation = ClaudeSessionRequest extends infer Request
  ? Request extends {
      action: "settings" | "command" | "rollback";
    }
    ? Omit<Request, "request_id">
    : never
  : never;

function isClaudeSessionState(
  value: ClaudeSessionResponse,
): value is ClaudeSessionState {
  return "turns" in value && Array.isArray(value.turns);
}

function isClaudeCommandsResponse(
  value: ClaudeSessionResponse,
): value is ClaudeCommandsResponse {
  return Array.isArray(value);
}

const valuesFrom = (
  value: Pick<ClaudeSessionState, "settings">,
  agent: Agent,
): ClaudeValues => ({
  permissionMode:
    value.settings?.permissionMode ||
    (agent.yoloMode === false ? "default" : "bypassPermissions"),
  thinking: value.settings?.thinking !== false,
  ...(value.settings?.autoCompactWindow
    ? { autoCompactWindow: Number(value.settings.autoCompactWindow) }
    : {}),
});

export function ClaudeSettings({
  agent,
  account,
  onSignIn,
}: {
  agent: Agent;
  account?: Account;
  onSignIn?: (accountKey: string) => void;
}) {
  const catalog = useWorkerModels(
    agent.accountKey || "default",
    !account || account.status === "ready",
  );
  const thinkingRequired = requiresThinking(agent.model || "", catalog.models);
  const [state, setState] = useState<ClaudeSessionState>({
    settings: {},
    turns: [],
    tasks: [],
  });
  const [stateLoaded, setStateLoaded] = useState(false);
  const [stateLoading, setStateLoading] = useState(true);
  const [loadAttempt, setLoadAttempt] = useState(0);
  const [commands, setCommands] = useState<ClaudeCommandsResponse>([]);
  const [values, setValues] = useState<ClaudeValues>(() =>
    valuesFrom({}, agent),
  );
  const valuesRef = useRef(values);
  const savedRef = useRef(values);
  const [window, setWindow] = useState("");
  const [command, setCommand] = useState("");
  const [turn, setTurn] = useState("");
  const [error, setError] = useState("");
  const [busyAction, setBusyAction] = useState(false);
  const [savingField, setSavingField] = useState("");
  const [savedLabel, setSavedLabel] = useState("");
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const windowSaveTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const call = (body: ClaudeSessionRequest) =>
    post("/api/claude/session", body, { timeoutMs: 15000 });
  const mutate = async (body: ClaudeSessionMutation) => {
    const storageKey = `claude-control:${agent.accountKey}:${agent.id}:${body.action}:${JSON.stringify({ ...body, id: undefined, action: undefined })}`;
    const requestId =
      saved<string | null>(storageKey, null) || crypto.randomUUID();
    localStorage.setItem(storageKey, JSON.stringify(requestId));
    const result = await call({ ...body, request_id: requestId });
    localStorage.removeItem(storageKey);
    return result;
  };
  const load = async () => {
    const result = await call({ id: agent.id, action: "state" });
    if (!isClaudeSessionState(result))
      throw new Error("Claude session state response is invalid.");
    const value = result;
    const next = valuesFrom(value, agent);
    setState(value);
    valuesRef.current = next;
    savedRef.current = next;
    setValues(next);
    setWindow(next.autoCompactWindow ? String(next.autoCompactWindow) : "");
  };
  useEffect(() => {
    let active = true;
    setStateLoaded(false);
    setStateLoading(true);
    setError("");
    call({ id: agent.id, action: "state" })
      .then((result) => {
        if (!active) return;
        if (!isClaudeSessionState(result))
          throw new Error("Claude session state response is invalid.");
        const value = result;
        const next = valuesFrom(value, agent);
        setState(value);
        valuesRef.current = next;
        savedRef.current = next;
        setValues(next);
        setWindow(next.autoCompactWindow ? String(next.autoCompactWindow) : "");
        setStateLoaded(true);
      })
      .catch((failure) => {
        if (!active) return;
        const message = errorText(failure);
        setError(
          message === "Sign in with claude auth login first"
            ? "Sign in to Claude to load these settings."
            : message,
        );
      })
      .finally(() => active && setStateLoading(false));
    return () => {
      active = false;
      if (timer.current) clearTimeout(timer.current);
      if (windowSaveTimer.current) clearTimeout(windowSaveTimer.current);
    };
  }, [agent.id, agent.accountKey, account?.status, loadAttempt]);

  const saveSetting = async (field: string, patch: Partial<ClaudeValues>) => {
    if (busyAction || savingField) return;
    const previous = savedRef.current;
    const next = { ...valuesRef.current, ...patch };
    valuesRef.current = next;
    setValues(next);
    setSavingField(field);
    setSavedLabel("");
    setError("");
    try {
      await mutate({ id: agent.id, action: "settings", settings: next });
      savedRef.current = next;
      setSavedLabel("Saved");
      if (timer.current) clearTimeout(timer.current);
      timer.current = setTimeout(() => setSavedLabel(""), 2200);
    } catch (failure) {
      valuesRef.current = previous;
      savedRef.current = previous;
      setValues(previous);
      setWindow(
        previous.autoCompactWindow ? String(previous.autoCompactWindow) : "",
      );
      setError(errorText(failure));
    } finally {
      setSavingField("");
    }
  };
  const run = async (work: () => Promise<unknown>, reload = false) => {
    setBusyAction(true);
    setError("");
    try {
      await work();
      if (reload) await load();
    } catch (failure) {
      setError(errorText(failure));
    } finally {
      setBusyAction(false);
    }
  };
  const saveWindow = (input = window) => {
    if (windowSaveTimer.current) clearTimeout(windowSaveTimer.current);
    if (
      input ===
      (values.autoCompactWindow ? String(values.autoCompactWindow) : "")
    )
      return;
    if (!input.trim()) {
      void saveSetting("Auto-compact limit", { autoCompactWindow: undefined });
      return;
    }
    const limit = Number(input);
    if (!Number.isInteger(limit) || limit < 100000 || limit > 1000000) {
      setWindow(
        values.autoCompactWindow ? String(values.autoCompactWindow) : "",
      );
      setError("Enter a limit from 100,000 to 1,000,000, or leave it blank.");
      return;
    }
    void saveSetting("Auto-compact limit", { autoCompactWindow: limit });
  };
  const historyTurns = [...state.turns];
  const recoveryTurn = state.controlOperation?.turnId;
  if (recoveryTurn && !historyTurns.some((item) => item.id === recoveryTurn))
    historyTurns.push({
      id: recoveryTurn,
      status: "interrupted",
      text: "Recover pending rollback",
    });
  useEffect(() => {
    if (turn && !historyTurns.some((item) => item.id === turn)) setTurn("");
  }, [state, turn]);
  const locked =
    !stateLoaded ||
    (!!account && account.status !== "ready") ||
    (typeof agent.status === "string" && busy.has(agent.status)) ||
    busyAction ||
    !!savingField;
  const nativeStatusError =
    agent.nativeStatus &&
    typeof agent.nativeStatus === "object" &&
    "error" in agent.nativeStatus &&
    typeof agent.nativeStatus.error === "string"
      ? agent.nativeStatus.error
      : undefined;

  return (
    <section
      className="settings-group claude-settings"
      aria-label="Claude settings"
      aria-busy={stateLoading}
    >
      <h2>Permissions</h2>
      {stateLoading && <p role="status">Loading Claude settings…</p>}
      <NativeSelect
        label="Permission mode"
        value={values.permissionMode}
        disabled={locked}
        onChange={(event) => {
          const selected = permissionMode(event.currentTarget.value);
          if (selected)
            void saveSetting("Permission mode", { permissionMode: selected });
        }}
        data={[
          { value: "default", label: "Ask for permission" },
          { value: "acceptEdits", label: "Allow file edits" },
          { value: "auto", label: "Automatic" },
          { value: "plan", label: "Plan only" },
          { value: "bypassPermissions", label: "Full access" },
        ]}
      />
      {thinkingRequired ? (
        <p className="claude-setting-help">
          Thinking is always on for this model.
        </p>
      ) : (
        <Switch
          label="Extended thinking"
          description="Controls how much reasoning the model uses."
          checked={values.thinking}
          disabled={locked || catalog.loading || !!catalog.error}
          onChange={(event) =>
            void saveSetting("Extended thinking", {
              thinking: event.currentTarget.checked,
            })
          }
        />
      )}
      <div className="claude-save-status" aria-live="polite">
        {savingField ? `Saving ${savingField.toLowerCase()}…` : savedLabel}
      </div>
      <details className="claude-advanced">
        <summary>Advanced</summary>
        <div className="claude-advanced-content">
          <TextInput
            label="Auto-compact token limit"
            description="Blank uses the Claude default."
            placeholder="Claude default"
            value={window}
            disabled={locked}
            onChange={(event) => {
              const input = event.currentTarget.value;
              setWindow(input);
              if (windowSaveTimer.current)
                clearTimeout(windowSaveTimer.current);
              windowSaveTimer.current = setTimeout(
                () => saveWindow(input),
                500,
              );
            }}
            onBlur={() => saveWindow()}
            onKeyDown={(event) => event.key === "Enter" && saveWindow()}
          />
          <details>
            <summary>Commands and skills</summary>
            <Button
              disabled={busyAction}
              onClick={() =>
                void run(async () => {
                  const result = await call({
                    id: agent.id,
                    action: "commands",
                  });
                  if (!isClaudeCommandsResponse(result))
                    throw new Error("Claude commands response is invalid.");
                  setCommands(result);
                })
              }
            >
              Load commands
            </Button>
            <NativeSelect
              label="Command"
              value={command}
              onChange={(event) => setCommand(event.currentTarget.value)}
              data={[
                { value: "", label: "Select a command" },
                ...commands.map((item) => ({
                  value: "/" + item.name,
                  label: "/" + item.name + " " + (item.description || ""),
                })),
              ]}
            />
            <TextInput
              label="Command and arguments"
              value={command}
              onChange={(event) => setCommand(event.currentTarget.value)}
            />
            <Button
              disabled={busyAction || !command}
              onClick={() =>
                void run(() =>
                  mutate({ id: agent.id, action: "command", command }),
                )
              }
            >
              Send command
            </Button>
            <Button
              disabled={locked}
              onClick={() =>
                void run(
                  () =>
                    mutate({
                      id: agent.id,
                      action: "command",
                      command: "/compact",
                    }),
                  true,
                )
              }
            >
              Compact conversation
            </Button>
          </details>
          <details>
            <summary>Conversation history</summary>
            <p className="claude-setting-help">
              Remove a turn and later turns from Claude’s context. Saved history
              remains.
            </p>
            <NativeSelect
              label="First turn to remove"
              value={turn}
              onChange={(event) => setTurn(event.currentTarget.value)}
              data={[
                { value: "", label: "Select a turn" },
                ...historyTurns.map((item, index) => ({
                  value: item.id,
                  label: `${index + 1}. ${item.text || item.status}`,
                })),
              ]}
            />
            <Button
              color="red"
              disabled={
                locked ||
                !turn ||
                !historyTurns.some((item) => item.id === turn)
              }
              onClick={() =>
                void run(
                  () =>
                    state.controlOperation?.turnId === turn
                      ? call({
                          id: agent.id,
                          action: "rollback",
                          turn_id: turn,
                          request_id: state.controlOperation.requestId,
                        })
                      : mutate({
                          id: agent.id,
                          action: "rollback",
                          turn_id: turn,
                        }),
                  true,
                )
              }
            >
              Roll back context
            </Button>
          </details>
          <p className="claude-version">
            Claude Code {state.version || "version unavailable"}
          </p>
          {(state.tasks || []).map((item) => (
            <div key={item.task_id} className="claude-task">
              <span>{item.description || item.task_id}</span>
              <Button
                disabled={busyAction}
                onClick={() =>
                  void run(
                    () =>
                      call({
                        id: agent.id,
                        action: "stop_task",
                        task_id: item.task_id,
                      }),
                    true,
                  )
                }
              >
                Stop task
              </Button>
            </div>
          ))}
        </div>
      </details>
      {error && (
        <p role="alert" className="claude-setting-error">
          {error}
        </p>
      )}
      {onSignIn && (
        <AccountSignInNotice
          account={account}
          errors={[
            error,
            typeof agent.error === "string" ? agent.error : undefined,
            nativeStatusError,
          ]}
          onSignIn={onSignIn}
        />
      )}
      {!stateLoaded && error && (
        <Button onClick={() => setLoadAttempt((value) => value + 1)}>
          Retry Claude settings
        </Button>
      )}
    </section>
  );
}

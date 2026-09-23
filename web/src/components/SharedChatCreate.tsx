import { Button, NativeSelect, TextInput } from "@mantine/core";
import { useEffect, useRef, useState } from "react";
import { api, ApiError, errorText, save, saved } from "../api";
import type { Json, Snapshot } from "../types";
import type { AccountsState } from "./Accounts";
import { useWorkerModels } from "./WorkerModelPicker";
import "./shared-chat-create.css";

type Participant = { account_key: string; model: string; effort?: string };
type Creation = { body: Json; roomId?: string; rejected?: boolean };
export const sharedCreationKey = (scope: string) =>
  `studio-radio-create:${scope}`;

function ParticipantFields({
  index,
  value,
  change,
  accounts,
  frozen,
  valid,
}: {
  index: number;
  value: Participant;
  change: (value: Participant) => void;
  accounts: AccountsState;
  frozen: boolean;
  valid: (ready: boolean) => void;
}) {
  const catalog = useWorkerModels(value.account_key, !!value.account_key);
  const options = catalog.models.map((row) => ({
    value: row.model,
    label: row.displayName || row.model,
  }));
  const info = catalog.models.find((row) => row.model === value.model);
  useEffect(() => {
    valid(!!info && !catalog.loading && !catalog.error);
    if (!frozen && !value.model && catalog.models.length) {
      const preferred =
        catalog.models.find((row) => row.isDefault) || catalog.models[0];
      change({ ...value, model: preferred.model });
    }
  }, [
    value.account_key,
    value.model,
    catalog.loading,
    catalog.error,
    !!info,
    frozen,
  ]);
  return (
    <fieldset className="shared-create-participant" disabled={frozen}>
      <legend>Agent {index + 1}</legend>
      <NativeSelect
        label={`Account for agent ${index + 1}`}
        value={value.account_key}
        data={[
          { value: "", label: "Select an account" },
          ...accounts.accounts
            .filter((a) => !a.disconnected && a.status === "ready")
            .map((a) => ({
              value: a.id,
              label: `${a.provider === "claude" ? "Claude" : "Codex"} · ${a.email || a.label}`,
            })),
        ]}
        onChange={(e) =>
          change({ account_key: e.currentTarget.value, model: "" })
        }
      />
      <NativeSelect
        label={`Model for agent ${index + 1}`}
        value={value.model}
        disabled={frozen || catalog.loading || !!catalog.error}
        data={[
          {
            value: "",
            label: catalog.loading ? "Loading models…" : "Select a model",
          },
          ...options,
        ]}
        onChange={(e) =>
          change({ ...value, model: e.currentTarget.value, effort: undefined })
        }
      />
      {!!info?.supportedReasoningEfforts?.length && (
        <NativeSelect
          label={`Reasoning for agent ${index + 1}`}
          value={value.effort || ""}
          data={[
            {
              value: "",
              label: `Default${info.defaultReasoningEffort ? ` (${info.defaultReasoningEffort})` : ""}`,
            },
            ...info.supportedReasoningEfforts.map((row: Json) => ({
              value: row.reasoningEffort,
              label: row.reasoningEffort,
            })),
          ]}
          onChange={(e) =>
            change({ ...value, effort: e.currentTarget.value || undefined })
          }
        />
      )}
      {catalog.error && (
        <div role="alert">
          {catalog.error}{" "}
          <Button size="compact-xs" onClick={catalog.retry}>
            Retry models
          </Button>
        </div>
      )}
    </fieldset>
  );
}
export default function SharedChatCreate({
  data,
  accounts,
  initialPath,
  refresh,
  created,
}: {
  data: Snapshot;
  accounts: AccountsState;
  initialPath?: string;
  refresh: () => Promise<void>;
  created: (id: string) => void;
}) {
  const key = sharedCreationKey(data.stateDir);
  const [attempt, setAttempt] = useState<Creation | null>(() =>
    saved(key, null),
  );
  const [path, setPath] = useState<string>(
    attempt?.body.path || initialPath || data.runtime.projects?.[0]?.path || "",
  );
  const [name, setName] = useState<string>(attempt?.body.name || "");
  const readyAccounts = accounts.accounts.filter(
    (a) => a.status === "ready" && !a.disconnected,
  );
  const first =
    readyAccounts.find((a) => a.id === accounts.defaultAccountKey) ||
    readyAccounts[0];
  const second =
    readyAccounts.find((a) => a.provider !== first?.provider) || first;
  const [participants, setParticipants] = useState<Participant[]>(
    attempt?.body.participants || [
      { account_key: first?.id || "", model: "" },
      { account_key: second?.id || "", model: "" },
    ],
  );
  useEffect(() => {
    if (attempt || !first) return;
    setParticipants((old) =>
      old.map((p, index) =>
        p.account_key ? p : { ...p, account_key: (index ? second : first)!.id },
      ),
    );
  }, [first?.id, second?.id, !!attempt]);
  const [valid, setValid] = useState([false, false]);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState("");
  const lock = useRef(false);
  const finished = useRef(false);
  const remember = (value: Creation | null) => {
    save(key, value);
    setAttempt(value);
  };
  useEffect(() => {
    if (
      !attempt?.roomId ||
      finished.current ||
      !data.runtime.rooms.some((room) => room.id === attempt.roomId)
    )
      return;
    finished.current = true;
    save(key, null);
    created(attempt.roomId);
  }, [attempt?.roomId, data.runtime.rooms, created, key]);
  const submit = async () => {
    if (lock.current) return;
    lock.current = true;
    setPending(true);
    setError("");
    let next = attempt || {
      body: {
        action: "radio",
        radio_action: "create",
        request_id: crypto.randomUUID(),
        path,
        name: name.trim() || "Shared chat",
        participants,
      },
    };
    remember(next);
    try {
      if (!next.roomId && !next.rejected) {
        const result = await api("/api/peer-teams", next.body, {
          timeoutMs: 15000,
        });
        if (!result.room?.id)
          throw Error("The server did not return the shared chat identity.");
        next = { ...next, roomId: result.room.id };
        remember(next);
      }
      await refresh();
      if (next.rejected) remember(null);
    } catch (e) {
      if (
        !next.roomId &&
        e instanceof ApiError &&
        e.status >= 400 &&
        e.status < 500
      ) {
        next = { ...next, rejected: true };
        remember(next);
      }
      setError(errorText(e));
    } finally {
      lock.current = false;
      setPending(false);
    }
  };
  const projects = [
    ...new Map(
      [
        ...(data.runtime.projects || []).map(
          (p) => [p.path, p.name || p.path] as const,
        ),
        ...data.threads
          .filter((a) => a.cwd)
          .map((a) => [a.cwd!, a.cwd!] as const),
      ].map(([path, label]) => [path, { value: path, label }]),
    ).values(),
  ];
  return (
    <form
      className="shared-chat-create"
      aria-label="Create shared chat"
      onSubmit={(e) => {
        e.preventDefault();
        void submit();
      }}
    >
      <p>
        One conversation with two agents. Both see every message and reply one
        at a time.
      </p>
      <NativeSelect
        label="Project"
        value={path}
        disabled={!!attempt}
        data={[{ value: "", label: "Select a project" }, ...projects]}
        onChange={(e) => setPath(e.currentTarget.value)}
      />
      <TextInput
        label="Chat name"
        placeholder="Optional"
        value={name}
        disabled={!!attempt}
        maxLength={80}
        onChange={(e) => setName(e.currentTarget.value)}
      />
      {participants.map((value, index) => (
        <ParticipantFields
          key={index}
          index={index}
          value={value}
          accounts={accounts}
          frozen={!!attempt}
          change={(next) =>
            setParticipants((old) =>
              old.map((p, i) => (i === index ? next : p)),
            )
          }
          valid={(ready) =>
            setValid((old) =>
              old[index] === ready
                ? old
                : old.map((v, i) => (i === index ? ready : v)),
            )
          }
        />
      ))}
      {!readyAccounts.length && (
        <p role="alert">Connect an account before you create a shared chat.</p>
      )}
      {attempt && (
        <p role={error ? "alert" : "status"}>
          {error}{" "}
          {attempt.roomId
            ? "Created. Refresh to open the chat."
            : attempt.rejected
              ? "Request rejected. Refresh before you edit."
              : "Waiting for confirmation. A retry uses the same request."}
        </p>
      )}
      <Button
        type="submit"
        loading={pending}
        disabled={
          pending ||
          (!attempt &&
            (!path ||
              !valid.every(Boolean) ||
              participants.some((p) => !p.model) ||
              participants.some(
                (p) => !readyAccounts.some((a) => a.id === p.account_key),
              )))
        }
      >
        {attempt
          ? attempt.roomId || attempt.rejected
            ? "Refresh"
            : "Retry creation"
          : "Create shared chat"}
      </Button>
    </form>
  );
}

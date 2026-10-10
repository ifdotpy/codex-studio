import { accountCanStart } from "../accountName";
import { ProviderMark, setupAccountName } from "./AccountTiles";
import { Button, NativeSelect, Popover, TextInput } from "@mantine/core";
import { useEffect, useRef, useState } from "react";
import { ApiError, errorText, post, save, saved, type PostBody } from "../api";
import type { Snapshot } from "../types";
import type { AccountsState } from "./Accounts";
import { useWorkerModels } from "./agents/WorkerModelPicker";
import { AgentSetupPicker, setupModelName } from "./agents/AgentSetupPicker";
import "./agents/execution-settings.css";
import "./shared-chat-create.css";

type Participant = {
  account_key: string;
  model: string;
  effort?: string | null;
};
type SharedCreateRequest = Extract<
  PostBody<"/api/peer-teams">,
  { action: "radio"; radio_action: "create" }
>;
type Creation = {
  body: SharedCreateRequest;
  roomId?: string;
  rejected?: boolean;
};
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
  const [opened, setOpened] = useState(false);
  const trigger = useRef<HTMLButtonElement>(null);
  const readyAccounts = accounts.accounts.filter((account) =>
    accountCanStart(account),
  );
  const account = accounts.accounts.find(
    (item) => item.id === value.account_key,
  );
  const provider = account?.provider || "codex";
  const [browsedProvider, setBrowsedProvider] = useState<string>(provider);
  useEffect(() => setBrowsedProvider(provider), [provider]);
  const providers = [
    ...new Set(readyAccounts.map((item) => item.provider || "codex")),
  ];
  const chooseAccount = (account_key: string | null) => {
    if (account_key && account_key !== value.account_key)
      change({ account_key, model: "" });
  };
  const info = catalog.models.find((row) => row.model === value.model);
  const reasoningEfforts =
    info?.supportedReasoningEfforts.map((item) => item.reasoningEffort) ?? [];
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
  const effort = value.effort || info?.defaultReasoningEffort || "Default";
  const summary = [
    setupModelName(info, value.model) ||
      (catalog.loading ? "Loading models…" : "Select a model"),
    effort[0].toUpperCase() + effort.slice(1),
    setupAccountName(account),
  ]
    .filter(Boolean)
    .join(" · ");
  return (
    <div
      className="shared-create-row shared-create-participant"
      onFocusCapture={(event) => {
        if (opened && event.target instanceof HTMLElement)
          event.target.setAttribute("data-mantine-stop-propagation", "true");
      }}
      onKeyDownCapture={(event) => {
        if (opened && event.key === "Escape") {
          event.preventDefault();
          event.stopPropagation();
          setOpened(false);
          trigger.current?.focus();
        }
      }}
    >
      <span>Agent {index + 1}</span>
      <Popover
        opened={opened && !frozen}
        onChange={setOpened}
        position="bottom-end"
        width={360}
        trapFocus
        returnFocus
      >
        <Popover.Target>
          <Button
            ref={trigger}
            type="button"
            className="shared-create-chip"
            aria-label={`Settings for agent ${index + 1}`}
            aria-expanded={opened && !frozen}
            data-mantine-stop-propagation={opened ? "true" : undefined}
            disabled={frozen}
            leftSection={<ProviderMark provider={provider} />}
            onClick={() => setOpened((old) => !old)}
          >
            <span>{summary}</span>
          </Button>
        </Popover.Target>
        <Popover.Dropdown className="execution-dropdown shared-create-picker">
          <AgentSetupPicker
            role="orchestrator"
            roles={[]}
            onRole={() => {}}
            provider={browsedProvider}
            providers={providers}
            onProvider={setBrowsedProvider}
            accounts={readyAccounts}
            accountKey={value.account_key}
            accountLabel={`Account for agent ${index + 1}`}
            onAccount={chooseAccount}
            models={catalog.models.map((row) => ({
              value: row.model,
              label: setupModelName(row),
              isDefault: row.isDefault,
            }))}
            model={value.model}
            modelLabel={`Model for agent ${index + 1}`}
            onModel={(model) => change({ ...value, model, effort: undefined })}
            efforts={
              reasoningEfforts.length
                ? [
                    { value: "", label: "Default" },
                    ...reasoningEfforts.map((item) => ({
                      value: item,
                      label: item[0].toUpperCase() + item.slice(1),
                    })),
                  ]
                : []
            }
            effort={value.effort || ""}
            effortLabel={`Reasoning for agent ${index + 1}`}
            onEffort={(next) => change({ ...value, effort: next || undefined })}
            disabled={frozen || catalog.loading || !!catalog.error}
            accountDisabled={frozen}
            fastAvailable={false}
            fast={false}
            onFast={() => {}}
            daybreakAvailable={false}
            daybreak={false}
            onDaybreak={() => {}}
          />
          {catalog.error && (
            <div role="alert">
              {catalog.error}{" "}
              <Button type="button" size="compact-xs" onClick={catalog.retry}>
                Retry models
              </Button>
            </div>
          )}
        </Popover.Dropdown>
      </Popover>
    </div>
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
    attempt?.body.path ||
      initialPath ||
      data.runtime?.projects?.[0]?.path ||
      "",
  );
  const [name, setName] = useState<string>(attempt?.body.name || "");
  const readyAccounts = accounts.accounts.filter((a) => accountCanStart(a));
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
      !data.runtime?.rooms.some((room) => room.id === attempt.roomId)
    )
      return;
    finished.current = true;
    save(key, null);
    created(attempt.roomId);
  }, [attempt?.roomId, data.runtime?.rooms, created, key]);
  const submit = async () => {
    if (lock.current) return;
    lock.current = true;
    setPending(true);
    setError("");
    const body: SharedCreateRequest = {
      action: "radio",
      radio_action: "create",
      request_id: crypto.randomUUID(),
      path,
      name: name.trim() || "Shared chat",
      participants,
    };
    let next: Creation = attempt || { body };
    remember(next);
    try {
      if (!next.roomId && !next.rejected) {
        const result = await post("/api/peer-teams", next.body, {
          timeoutMs: 15000,
        });
        const room = "room" in result ? result.room : null;
        if (
          !room ||
          typeof room !== "object" ||
          Array.isArray(room) ||
          !("id" in room) ||
          typeof room.id !== "string"
        )
          throw Error("The server did not return the shared chat identity.");
        next = { ...next, roomId: room.id };
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
        ...(data.runtime?.projects || []).flatMap((project) => {
          const projectPath = project.path;
          if (typeof projectPath !== "string" || !projectPath) return [];
          const label = project.name;
          return [
            [
              projectPath,
              typeof label === "string" && label
                ? label
                : projectPath.split("/").filter(Boolean).pop() || projectPath,
            ] as const,
          ];
        }),
        ...data.threads.flatMap((thread) =>
          typeof thread.cwd === "string" && thread.cwd
            ? [[thread.cwd, thread.cwd] as const]
            : [],
        ),
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
      {(projects.length > 1 || !path) && (
        <div className="shared-create-row">
          <span>Project</span>
          <NativeSelect
            aria-label="Project"
            value={path}
            disabled={!!attempt}
            data={[{ value: "", label: "Select a project" }, ...projects]}
            onChange={(e) => setPath(e.currentTarget.value)}
          />
        </div>
      )}
      <div className="shared-create-row">
        <span>Name</span>
        <TextInput
          aria-label="Chat name"
          placeholder="Optional"
          value={name}
          disabled={!!attempt}
          maxLength={80}
          onChange={(e) => setName(e.currentTarget.value)}
        />
      </div>
      <div className="shared-create-participants">
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
      </div>
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
      <div className="shared-create-footer">
        <Button
          variant="filled"
          color="indigo"
          aria-label={attempt ? undefined : "Create"}
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
            : "Create"}
        </Button>
      </div>
    </form>
  );
}

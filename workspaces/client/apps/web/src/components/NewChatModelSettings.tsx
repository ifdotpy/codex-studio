import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Button } from "@mantine/core";
import { errorText, type GetResult } from "../api";
import type { Agent } from "../types";
import type { StudioServer } from "../servers/registry";
import { ExecutionSettings } from "./agents/ExecutionSettings";
import {
  ModelCatalogReader,
  useWorkerModels,
} from "./agents/WorkerModelPicker";
import { readNewChatResource, type NewChatSettings } from "./newChatSettings";

type Props = {
  server?: StudioServer;
  projectId: string;
  path: string;
  onOpenChange: (opened: boolean) => void;
  onChange: (settings: Omit<NewChatSettings, "workspaceMode"> | null) => void;
};
export default function NewChatModelSettings(props: Props) {
  const reader = useMemo(
    () => ({
      scope: props.server?.id || "current",
      read: (
        options: Parameters<typeof readNewChatResource<"/api/models">>[2],
      ) => readNewChatResource(props.server, "/api/models", options),
    }),
    [props.server],
  );
  return (
    <ModelCatalogReader.Provider value={reader}>
      <ModelDraft {...props} />
    </ModelCatalogReader.Provider>
  );
}

function ModelDraft({
  server,
  projectId,
  path,
  onChange,
  onOpenChange,
}: Props) {
  const openRoles = useRef({ orchestrator: false, worker: false });
  const openCallbacks = useMemo(
    () =>
      Object.fromEntries(
        (["orchestrator", "worker"] as const).map((role) => [
          role,
          (opened: boolean) => {
            openRoles.current[role] = opened;
            onOpenChange(Object.values(openRoles.current).some(Boolean));
          },
        ]),
      ),
    [onOpenChange],
  );
  const [accounts, setAccounts] = useState<GetResult<"/api/accounts"> | null>(
    null,
  );
  const [accountKey, setAccountKey] = useState("");
  const [settings, setSettings] = useState<
    Omit<NewChatSettings, "workspaceMode">
  >({});
  const [error, setError] = useState("");
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    const controller = new AbortController();
    setError("");
    onChange(null);
    void Promise.all([
      readNewChatResource(server, "/api/accounts", {
        signal: controller.signal,
      }),
      readNewChatResource(server, "/api/projects", {
        signal: controller.signal,
      }),
    ])
      .then(([value, projects]) => {
        if (controller.signal.aborted) return;
        const project = projects.items.find(
          (row) => row.id === projectId || row.path === path,
        );
        setAccounts(value);
        setAccountKey(
          project?.accountKey || value.defaultAccountKey || "default",
        );
      })
      .catch((failure) => {
        if (!controller.signal.aborted) setError(errorText(failure));
      });
    return () => controller.abort();
  }, [server, projectId, path, attempt, onChange]);
  const catalog = useWorkerModels(accountKey || "default", !!accounts);
  const model =
    settings.model ||
    catalog.models.find((row) => row.model === "gpt-6-astra")?.model ||
    catalog.models.find((row) => row.isDefault)?.model ||
    catalog.models[0]?.model;
  const worker = settings.worker_defaults;
  const workerModel = catalog.models.some((row) => row.model === "gpt-6-luna")
    ? "gpt-6-luna"
    : model;
  const agent: Agent = {
    id: "new-chat-draft",
    name: "New chat",
    isLead: true,
    source: "managed",
    status: "idle",
    accountKey,
    provider: accounts?.accounts.find((row) => row.id === accountKey)?.provider,
    model,
    effort: settings.effort === undefined ? "medium" : settings.effort,
    fastMode: !!settings.fast_mode,
    daybreakEnabled: !!settings.daybreak_enabled,
    workerDefaults: {
      model: worker?.model === undefined ? workerModel : worker.model,
      effort: worker?.effort === undefined ? "high" : worker.effort,
      fastMode: !!worker?.fast_mode,
      daybreakEnabled: !!worker?.daybreak_enabled,
      accountKey: worker?.account_key,
    },
    reviewDefaults: settings.review_defaults,
  };
  const modelError =
    catalog.error ||
    (accounts && !catalog.loading && !model
      ? "No models are available for this account."
      : "");
  const ready =
    !!accounts && !catalog.loading && !modelError && !!model && !error;
  useEffect(() => {
    onChange(ready ? { ...settings, model } : null);
  }, [ready, settings, model, onChange]);
  const patch = useCallback(
    (value: Partial<import("../api").PostBody<"/api/conversation">>) => {
      const { id: _id, expected_account_key: _expected, ...values } = value;
      setSettings((previous) => ({ ...previous, ...values }));
    },
    [],
  );
  return (
    <div
      className="empty-chat-settings new-chat-model-settings"
      aria-label="Agent settings"
    >
      {(["orchestrator", "worker"] as const).map((role) => (
        <div className="empty-chat-setting" key={role}>
          <span>{role === "orchestrator" ? "Model" : "Workers"}</span>
          <ExecutionSettings
            agent={agent}
            catalog={catalog}
            accounts={accounts?.accounts || []}
            refresh={async () => {}}
            settingsRow
            showAccountSummary
            initialRole={role}
            onOpenChange={openCallbacks[role]}
            onDraftChange={patch}
            onAccountChange={(key) => {
              setAccountKey(key);
              setSettings((previous) => ({
                ...previous,
                account_key: key,
                model: undefined,
                effort: null,
                fast_mode: false,
                daybreak_enabled: false,
              }));
            }}
          />
        </div>
      ))}
      {(error || modelError) && (
        <div role="alert">
          {error || modelError}
          <Button
            size="xs"
            onClick={() => {
              setAttempt((value) => value + 1);
              catalog.retry();
            }}
          >
            Retry
          </Button>
        </div>
      )}
    </div>
  );
}

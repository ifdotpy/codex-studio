import { isModelWhitelisted, useModelWhitelist } from "./models/modelWhitelist";
import { modelForValue } from "./models/modelOption";
import { useServerServices } from "../servers/ServerServicesContext";
import { createServerServices } from "../servers/serverServices";
import { watchResourceReads } from "./watchResourceReads";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Button, Stack, Text } from "@mantine/core";
import { errorText, type GetResult } from "../api";
import type { Agent } from "../types";
import { serverResourceScope, type StudioServer } from "../servers/registry";
import { ExecutionSettings, settingsFor } from "./agents/ExecutionSettings";
import {
  ModelCatalogReader,
  useWorkerModels,
} from "./agents/WorkerModelPicker";
import {
  DEFAULT_CLAUDE_LEAD_EFFORT,
  DEFAULT_CLAUDE_LEAD_MODEL,
} from "../generated/sharedValues";
import { defaultChatModel, newChatDefault } from "./accountPreference";
import {
  readNewChatResource,
  settingsForNewChatRequest,
  settingsForNewChatAccount,
  type NewChatSettings,
} from "./newChatSettings";

type Props = {
  server?: StudioServer;
  projectId: string;
  path: string;
  onChange: (settings: Omit<NewChatSettings, "workspaceMode"> | null) => void;
};
// The parent can pass a new object for the same server. Reads restart only for another
// server identity, so a rebuilt list does not reset the user's account or models.
const serverIdentity = (server?: StudioServer) =>
  JSON.stringify([server?.id, server?.origin, server?.credentialId]);

export default function NewChatModelSettings(props: Props) {
  const inherited = useServerServices();
  const owner = props.server ?? inherited?.owner;
  const serverKey = serverIdentity(owner);
  const services = useMemo(() => {
    if (!owner) return undefined;
    return inherited && serverIdentity(inherited.owner) === serverKey
      ? inherited
      : createServerServices(owner);
  }, [serverKey, inherited]);
  useEffect(
    () => () => {
      if (services && services !== inherited) services.close();
    },
    [services, inherited],
  );
  const server = useRef(owner);
  server.current = owner;
  const reader = useMemo(() => {
    // Several pickers read the same catalog. One read per query keeps the slow server and
    // the browser's few connections for each server free for other requests.
    type Options = Parameters<typeof readNewChatResource<"/api/models">>[2];
    type Catalog = ReturnType<typeof readNewChatResource<"/api/models">>;
    const reads = new Map<string, Catalog>();
    return {
      cacheOwner: services,
      scope: services
        ? JSON.stringify([
            services.owner.id,
            services.owner.origin,
            services.owner.credentialId,
          ])
        : serverResourceScope(server.current),
      watchResourceReads: services?.watchResourceReads ?? watchResourceReads,
      read: (options: Options) => {
        const key = JSON.stringify(options?.query ?? {});
        let shared = reads.get(key);
        if (!shared) {
          shared = services
            ? services.api.get("/api/models", { query: options?.query })
            : readNewChatResource(server.current, "/api/models", {
                query: options?.query,
              });
          reads.set(key, shared);
          void shared.catch(() => {}).finally(() => reads.delete(key));
        }
        const signal = options?.signal;
        if (!signal) return shared;
        return new Promise((resolve, reject) => {
          const abort = () => reject(new DOMException("Aborted", "AbortError"));
          if (signal.aborted) return abort();
          signal.addEventListener("abort", abort, { once: true });
          shared.then(resolve, reject);
        }) as Catalog;
      },
    };
  }, [serverKey, services]);
  return (
    <ModelCatalogReader.Provider value={reader}>
      <ModelDraft {...props} server={owner} />
    </ModelCatalogReader.Provider>
  );
}

function ModelDraft({ server, projectId, path, onChange }: Props) {
  const [accounts, setAccounts] = useState<GetResult<"/api/accounts"> | null>(
    null,
  );
  const [accountKey, setAccountKey] = useState("");
  const [settings, setSettings] = useState<
    Omit<NewChatSettings, "workspaceMode">
  >({});
  const [error, setError] = useState("");
  const [attempt, setAttempt] = useState(0);
  const serverKey = serverIdentity(server);
  const currentServer = useRef(server);
  currentServer.current = server;
  useEffect(() => {
    const controller = new AbortController();
    setError("");
    onChange(null);
    void Promise.all([
      readNewChatResource(currentServer.current, "/api/accounts", {
        signal: controller.signal,
      }),
      readNewChatResource(currentServer.current, "/api/projects", {
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
          newChatDefault(
            value.accounts,
            value.defaultAccountKey || "default",
            value.defaultAccountGroups,
            project,
            null,
          ).accountKey,
        );
      })
      .catch((failure) => {
        if (!controller.signal.aborted) setError(errorText(failure));
      });
    return () => controller.abort();
  }, [serverKey, projectId, path, attempt, onChange]);
  const catalog = useWorkerModels(accountKey || "default", !!accounts);
  const provider = accounts?.accounts.find(
    (row) => row.id === accountKey,
  )?.provider;
  const allowed = useModelWhitelist();
  const availableModels = catalog.models.filter((row) =>
    isModelWhitelisted({ ...row, provider: row.provider || provider }, allowed),
  );
  const model = settings.model || defaultChatModel(provider, availableModels);
  const modelInfo = modelForValue(catalog.models, model);
  // A Claude orchestrator that keeps the default model starts at the default effort.
  const defaultEffort =
    !settings.model &&
    settings.effort === undefined &&
    model === DEFAULT_CLAUDE_LEAD_MODEL
      ? DEFAULT_CLAUDE_LEAD_EFFORT
      : undefined;
  const worker = settings.worker_defaults;
  const defaultWorker = settingsFor({}, true);
  const agent: Agent = {
    id: "new-chat-draft",
    name: "New chat",
    isLead: true,
    source: "managed",
    status: "idle",
    accountKey,
    provider,
    model,
    effort: settings.effort === undefined ? "medium" : settings.effort,
    fastMode: !!settings.fast_mode,
    daybreakEnabled: !!settings.daybreak_enabled,
    workerDefaults: {
      model: worker?.model === undefined ? defaultWorker.model : worker.model,
      effort:
        worker?.effort === undefined ? defaultWorker.effort : worker.effort,
      fastMode: !!worker?.fast_mode,
      daybreakEnabled: !!worker?.daybreak_enabled,
      accountKey: worker?.account_key,
    },
    reviewDefaults: settings.review_defaults,
  };
  const modelError =
    catalog.error ||
    (accounts && !catalog.loading && !model
      ? "No enabled models are available for this account. Select models in Studio settings → Models."
      : modelInfo &&
          !isModelWhitelisted(
            { ...modelInfo, provider: modelInfo.provider || provider },
            allowed,
          )
        ? "Choose a model enabled in Studio settings → Models."
        : "");
  const ready =
    !!accounts && !catalog.loading && !modelError && !!model && !error;
  useEffect(() => {
    onChange(
      ready
        ? settingsForNewChatRequest(
            {
              ...settings,
              ...(defaultEffort ? { effort: defaultEffort } : {}),
            },
            accountKey,
            settings.account_key !== undefined,
            model,
          )
        : null,
    );
  }, [ready, settings, defaultEffort, accountKey, model, onChange]);
  const patch = useCallback(
    (value: Partial<import("../api").PostBody<"/api/conversation">>) => {
      const { id: _id, expected_account_key: _expected, ...values } = value;
      setSettings((previous) => ({ ...previous, ...values }));
    },
    [],
  );
  return (
    <Stack
      gap="lg"
      className="new-chat-model-settings"
      aria-label="Agent settings"
    >
      {(["orchestrator", "worker"] as const).map((role) => (
        <section
          key={role}
          aria-label={role === "orchestrator" ? "Lead model" : "Worker model"}
        >
          <Text fw={600} mb="xs">
            {role === "orchestrator" ? "Lead" : "Workers"}
          </Text>
          <ExecutionSettings
            agent={agent}
            catalog={catalog}
            accounts={accounts?.accounts || []}
            refresh={async () => {}}
            presentation="panel"
            settingsRow
            showAccountSummary
            initialRole={role}
            onDraftChange={patch}
            onAccountChange={(key) => {
              setAccountKey(key);
              setSettings((previous) =>
                settingsForNewChatAccount(previous, key, model),
              );
            }}
          />
        </section>
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
    </Stack>
  );
}

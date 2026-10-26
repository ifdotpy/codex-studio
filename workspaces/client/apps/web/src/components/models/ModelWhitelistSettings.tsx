import {
  useServerStorage,
  useServerServices,
} from "../../servers/ServerServicesContext";
import { useServerSettings } from "../../servers/ServerSettingsContext";
import { Button, Checkbox, Group, Stack, Text } from "@mantine/core";
import { useCallback, useState, useSyncExternalStore } from "react";
import {
  readPreferenceWriteStatus,
  subscribePreferenceWriteStatus,
  retryPreferenceWrites,
} from "../../sync/uiPreferenceStatus";
import "./model-whitelist-settings.css";
import { useWorkerModels } from "../agents/WorkerModelPicker";
import { toModelOptions } from "./modelOption";
import {
  isModelWhitelisted,
  modelWhitelistKey,
  modelWhitelistReadError,
  readModelWhitelist,
  useModelWhitelist,
  writeModelWhitelist,
  type ModelWhitelist,
} from "./modelWhitelist";

export function ModelWhitelistSettings() {
  const services = useServerServices();
  const settings = useServerSettings();
  const owner = services?.owner || settings?.server;
  const catalog = useWorkerModels("default", true, true);
  const allowed = useModelWhitelist();
  const storage = useServerStorage();
  const readStatus = useCallback(
    () => readPreferenceWriteStatus(storage, "server"),
    [storage],
  );
  const status = useSyncExternalStore(
    subscribePreferenceWriteStatus,
    readStatus,
    readStatus,
  );
  const [error, setError] = useState("");
  const savedError = modelWhitelistReadError(storage);
  const models = [
    ...new Map(
      toModelOptions(catalog.models.filter((row) => !row.hidden)).map(
        (model) => [modelWhitelistKey(model), model],
      ),
    ).values(),
  ];
  const save = (value: ModelWhitelist) => {
    try {
      writeModelWhitelist(value, storage);
      setError("");
    } catch (failure) {
      setError(String(failure));
    }
  };
  return (
    <Stack
      gap="sm"
      className="model-whitelist-settings"
      aria-label="Models whitelist"
    >
      <Text fw={600}>Models on {owner?.label || "Local"}</Text>
      <Text size="sm">
        Choose the models shown in this server’s model pickers. Existing chats
        keep their model.
      </Text>
      <Text size="xs" c="dimmed" role="status" aria-live="polite">
        {status.state === "saving"
          ? "Saving model selection…"
          : status.state === "error"
            ? "Model selection is not saved."
            : "Model selection saved on this server."}
      </Text>
      {status.state === "error" && (
        <>
          <Text role="alert" c="red">
            {status.error}
          </Text>
          <Button
            variant="default"
            onClick={() => retryPreferenceWrites(storage)}
          >
            Retry saving
          </Button>
        </>
      )}
      <Group gap="sm">
        <Button variant="default" onClick={() => save(null)}>
          Select all models
        </Button>
        <Button
          variant="subtle"
          disabled={!models.length}
          onClick={() => save([])}
        >
          Clear selection
        </Button>
      </Group>
      {allowed === null && (
        <Text size="xs" c="dimmed">
          All models selected, including new models.
        </Text>
      )}
      {allowed?.length === 0 && (
        <Text size="sm">Select a model to start new chats.</Text>
      )}
      {catalog.loading && <Text role="status">Loading models…</Text>}
      {(error || savedError || catalog.error) && (
        <Text role="alert" c="red">
          {error || savedError || catalog.error}
        </Text>
      )}
      {catalog.error && (
        <Button variant="default" onClick={catalog.retry}>
          Retry
        </Button>
      )}
      {[...new Set(models.map((model) => model.provider))].map((provider) => (
        <Stack key={provider} gap="xs" className="model-whitelist-provider">
          <Text fw={600}>
            {provider === "claude"
              ? "Claude"
              : provider === "codex"
                ? "Codex"
                : provider}
          </Text>
          <div className="model-whitelist-grid">
            {models
              .filter((model) => model.provider === provider)
              .map((model) => {
                const key = modelWhitelistKey(model);
                return (
                  <Checkbox
                    key={key}
                    label={model.label}
                    checked={isModelWhitelisted(model, allowed)}
                    onChange={(event) => {
                      const current =
                        readModelWhitelist(storage) ??
                        models.map(modelWhitelistKey);
                      save(
                        event.currentTarget.checked
                          ? [...current, key]
                          : current.filter((value) => value !== key),
                      );
                    }}
                  />
                );
              })}
          </div>
        </Stack>
      ))}
    </Stack>
  );
}

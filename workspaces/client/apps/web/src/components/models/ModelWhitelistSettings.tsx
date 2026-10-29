import { providerLabel } from "../../generated/providerKind.ts";
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
import { ModelChoice } from "../../generated/api";
import {
  isModelEnabled,
  modelChoiceKey,
  readModelChoices,
  useModelChoices,
  writeModelChoices,
} from "./modelChoices";

export function ModelWhitelistSettings() {
  const services = useServerServices();
  const settings = useServerSettings();
  const owner = services?.owner || settings?.server;
  const catalog = useWorkerModels("default", true, true);
  const choices = useModelChoices();
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
  const models = [
    ...new Map(
      toModelOptions(catalog.models.filter((row) => !row.hidden)).map(
        (model) => [modelChoiceKey(model), model],
      ),
    ).values(),
  ];
  const save = (keys: string[], choice: ModelChoice) => {
    try {
      writeModelChoices(
        {
          ...readModelChoices(storage),
          ...Object.fromEntries(keys.map((key) => [key, choice])),
        },
        storage,
      );
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
        <Button
          variant="default"
          disabled={!models.length}
          onClick={() =>
            save(models.map(modelChoiceKey), ModelChoice.whitelisted)
          }
        >
          Select all models
        </Button>
        <Button
          variant="subtle"
          disabled={!models.length}
          onClick={() =>
            save(models.map(modelChoiceKey), ModelChoice.blacklisted)
          }
        >
          Clear selection
        </Button>
      </Group>
      <Text size="xs" c="dimmed">
        New models appear enabled until you hide them.
      </Text>
      {models.length > 0 &&
        !models.some((model) => isModelEnabled(model, choices)) && (
          <Text size="sm">Select a model to start new chats.</Text>
        )}
      {catalog.loading && <Text role="status">Loading models…</Text>}
      {(error || catalog.error) && (
        <Text role="alert" c="red">
          {error || catalog.error}
        </Text>
      )}
      {catalog.error && (
        <Button variant="default" onClick={catalog.retry}>
          Retry
        </Button>
      )}
      {[...new Set(models.map((model) => model.provider))].map((provider) => (
        <Stack key={provider} gap="xs" className="model-whitelist-provider">
          <Text fw={600}>{providerLabel(provider)}</Text>
          <div className="model-whitelist-grid">
            {models
              .filter((model) => model.provider === provider)
              .map((model) => {
                const key = modelChoiceKey(model);
                return (
                  <Checkbox
                    key={key}
                    label={model.label}
                    checked={isModelEnabled(model, choices)}
                    onChange={(event) =>
                      save(
                        [key],
                        event.currentTarget.checked
                          ? ModelChoice.whitelisted
                          : ModelChoice.blacklisted,
                      )
                    }
                  />
                );
              })}
          </div>
        </Stack>
      ))}
    </Stack>
  );
}

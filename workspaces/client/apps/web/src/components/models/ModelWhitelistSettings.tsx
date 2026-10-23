import { Button, Checkbox, Group, Stack, Text } from "@mantine/core";
import { useState } from "react";
import { useWorkerModels } from "../agents/WorkerModelPicker";
import { toModelOptions } from "./modelOption";
import {
  isModelWhitelisted,
  modelWhitelistKey,
  useModelWhitelist,
  writeModelWhitelist,
  type ModelWhitelist,
} from "./modelWhitelist";

export function ModelWhitelistSettings() {
  const catalog = useWorkerModels("default", true, true);
  const allowed = useModelWhitelist();
  const [error, setError] = useState("");
  const models = [
    ...new Map(
      toModelOptions(catalog.models.filter((row) => !row.hidden)).map(
        (model) => [modelWhitelistKey(model), model],
      ),
    ).values(),
  ];
  const save = (value: ModelWhitelist) => {
    try {
      writeModelWhitelist(value);
      setError("");
    } catch (failure) {
      setError(String(failure));
    }
  };
  return (
    <Stack gap="md" aria-label="Models whitelist">
      <Text size="sm">
        Choose the models shown in model pickers. Existing chats keep their
        model.
      </Text>
      <Text size="xs" c="dimmed">
        For this session only. This experimental preference resets when Studio
        reloads.
      </Text>
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
        <Stack key={provider} gap="xs">
          <Text fw={600}>
            {provider === "claude"
              ? "Claude"
              : provider === "codex"
                ? "Codex"
                : provider}
          </Text>
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
                    const current = allowed ?? models.map(modelWhitelistKey);
                    save(
                      event.currentTarget.checked
                        ? [...current, key]
                        : current.filter((value) => value !== key),
                    );
                  }}
                />
              );
            })}
        </Stack>
      ))}
    </Stack>
  );
}

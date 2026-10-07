import { Button, NumberInput, Stack, Textarea, TextInput } from "@mantine/core";
import { useRef, useState } from "react";
import { errorText, post, type PostBody, type PostResult } from "../api";
import type { Account } from "./Accounts";

type ClaudeProfileOptions = PostBody<"/api/claude/profiles">["options"];
type ClaudeProfileResult = PostResult<"/api/claude/profiles">;

export default function ClaudeProfile({
  account,
  onSaved,
}: {
  account?: Account;
  onSaved: (state: ClaudeProfileResult) => void;
}) {
  const initial = account?.claudeOptions || {};
  const [label, setLabel] = useState(account?.label || "Claude Code");
  const [binaryPath, setBinaryPath] = useState(initial.binaryPath || "");
  const [configDir, setConfigDir] = useState(initial.configDir || "");
  const [launchArgs, setLaunchArgs] = useState(initial.launchArgs || "");
  const [window, setWindow] = useState<string | number>(
    initial.autoCompactWindow || "",
  );
  const [models, setModels] = useState(
    (initial.customModels || [])
      .map(
        (model) =>
          `${model.id}${model.label && model.label !== model.id ? ` | ${model.label}` : ""}`,
      )
      .join("\n"),
  );
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [saved, setSaved] = useState(false);
  const lock = useRef(false);
  const save = async () => {
    if (lock.current) return;
    lock.current = true;
    setBusy(true);
    setError("");
    setSaved(false);
    try {
      const options: ClaudeProfileOptions = {
        ...initial,
        binaryPath: binaryPath.trim(),
        configDir: configDir.trim(),
        launchArgs: launchArgs.trim(),
        customModels: models
          .split("\n")
          .filter((line: string) => line.trim())
          .map((line: string) => {
            const [id, ...parts] = line.split("|").map((part) => part.trim());
            return { id, label: parts.join(" | ") || id };
          }),
      };
      if (window === "") delete options.autoCompactWindow;
      else options.autoCompactWindow = Number(window);
      const result = await post("/api/claude/profiles", {
        options,
        label: label.trim(),
        ...(account ? { account_key: account.id } : {}),
      });
      onSaved(result);
      setSaved(true);
    } catch (failure) {
      setError(errorText(failure));
    } finally {
      lock.current = false;
      setBusy(false);
    }
  };
  return (
    <details className="account-register claude-profile">
      <summary>{account ? "Configure Claude" : "Claude"}</summary>
      <form
        onChange={() => setSaved(false)}
        onSubmit={(event) => {
          event.preventDefault();
          void save();
        }}
      >
        <Stack gap="sm" w="100%">
          <small>
            {account
              ? "To change these paths, add another profile."
              : "Sign in to this Claude Code configuration with your subscription before you add it."}
          </small>
          {account && <strong>{account.email || account.label}</strong>}
          <TextInput
            label="Profile name"
            value={label}
            maxLength={200}
            required
            onChange={(event) => setLabel(event.currentTarget.value)}
            disabled={busy}
          />
          <TextInput
            label="Claude executable"
            placeholder="Installed Claude Code"
            value={binaryPath}
            onChange={(event) => setBinaryPath(event.currentTarget.value)}
            readOnly={!!account}
            disabled={busy}
          />
          <TextInput
            label="Claude configuration directory"
            placeholder="Default Claude configuration"
            value={configDir}
            onChange={(event) => setConfigDir(event.currentTarget.value)}
            readOnly={!!account}
            disabled={busy}
          />
          <fieldset className="claude-advanced">
            <legend>Advanced</legend>
            <NumberInput
              label="Automatic compaction threshold (tokens)"
              placeholder="Claude default"
              value={window}
              onChange={setWindow}
              min={100000}
              max={1000000}
              allowDecimal={false}
              disabled={busy}
            />
            <Textarea
              label="Custom models"
              description="One model ID per line. Add | and a display name if needed."
              value={models}
              onChange={(event) => setModels(event.currentTarget.value)}
              autosize
              minRows={2}
              disabled={busy}
            />
            <TextInput
              label="Launch arguments"
              placeholder="--add-dir /path/to/project"
              value={launchArgs}
              onChange={(event) => setLaunchArgs(event.currentTarget.value)}
              disabled={busy}
            />
          </fieldset>
          {error && (
            <p className="account-action-error" role="alert">
              {error}
            </p>
          )}
          {saved && <p role="status">Claude profile saved.</p>}
          <Button
            variant="filled"
            color="indigo"
            type="submit"
            loading={busy}
            disabled={!label.trim() || busy}
          >
            {account ? "Save Claude profile" : "Add Claude profile"}
          </Button>
        </Stack>
      </form>
    </details>
  );
}

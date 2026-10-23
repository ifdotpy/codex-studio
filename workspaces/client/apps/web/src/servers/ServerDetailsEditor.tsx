import { useEffect, useRef, useState } from "react";
import { TextInput } from "@mantine/core";
import { ActionButton } from "../components/ui/primitives";
import { validateServerName } from "./serverNames";
import { validateServerAlias } from "./serverAliases";
import { errorText } from "../api";

/** Edits share a form; each field keeps its own confirmed value and save owner. */
export default function ServerDetailsEditor({
  name,
  alias,
  usedAliases,
  disabled,
  saveName,
  saveAlias,
}: {
  name: string;
  alias: string;
  usedAliases: string[];
  disabled?: boolean;
  saveName?: (value: string) => Promise<boolean>;
  saveAlias: (value: string) => Promise<boolean>;
}) {
  const [label, setLabel] = useState(name);
  const [shortName, setShortName] = useState(alias);
  const [savedName, setSavedName] = useState(name);
  const [savedAlias, setSavedAlias] = useState(alias);
  const [nameError, setNameError] = useState("");
  const [aliasError, setAliasError] = useState("");
  const [nameSaved, setNameSaved] = useState(false);
  const [aliasSaved, setAliasSaved] = useState(false);
  const [busy, setBusy] = useState(false);
  const lock = useRef(false);
  useEffect(() => {
    setLabel(name);
    setSavedName(name);
  }, [name]);
  useEffect(() => {
    setShortName(alias);
    setSavedAlias(alias);
  }, [alias]);
  const nameDirty = !!saveName && label !== savedName;
  const aliasDirty = shortName !== savedAlias;
  const save = async () => {
    if (lock.current || disabled || (!nameDirty && !aliasDirty)) return;
    let invalid = false;
    setNameError("");
    setAliasError("");
    if (nameDirty) {
      try {
        validateServerName(label);
      } catch (error) {
        setNameError(errorText(error));
        invalid = true;
      }
    }
    if (aliasDirty) {
      try {
        validateServerAlias(shortName, usedAliases);
      } catch (error) {
        setAliasError(errorText(error));
        invalid = true;
      }
    }
    if (invalid) return;
    lock.current = true;
    setBusy(true);
    try {
      if (nameDirty && saveName) {
        try {
          if (!(await saveName(label))) {
            setNameError("Check the server name or retry the saved request.");
            return;
          }
          setSavedName(label);
          setNameSaved(true);
        } catch (error) {
          setNameError(errorText(error));
          return;
        }
      }
      if (aliasDirty) {
        try {
          if (!(await saveAlias(shortName))) {
            setAliasError(
              "The alias result is unknown. Check server settings or retry the saved request.",
            );
            return;
          }
          setSavedAlias(shortName);
          setAliasSaved(true);
        } catch (error) {
          setAliasError(errorText(error));
        }
      }
    } finally {
      lock.current = false;
      setBusy(false);
    }
  };
  return (
    <details className="server-settings-edit">
      <summary>Edit</summary>
      <form
        onSubmit={(event) => {
          event.preventDefault();
          void save();
        }}
      >
        <div className="server-settings-edit-fields">
          {saveName && (
            <div>
              <TextInput
                label="Server name"
                description="Use 1 to 80 visible characters."
                value={label}
                error={nameError}
                disabled={disabled || busy}
                autoComplete="off"
                onChange={(event) => {
                  setLabel(event.currentTarget.value);
                  setNameError("");
                  setNameSaved(false);
                }}
              />
              {nameSaved && !nameDirty && (
                <small role="status">Name saved.</small>
              )}
            </div>
          )}
          <div>
            <TextInput
              label="Server alias"
              description="Use 1 to 3 uppercase letters."
              value={shortName}
              error={aliasError}
              disabled={disabled || busy}
              autoComplete="off"
              onChange={(event) => {
                setShortName(event.currentTarget.value);
                setAliasError("");
                setAliasSaved(false);
              }}
            />
            {aliasSaved && !aliasDirty && (
              <small role="status">Alias saved.</small>
            )}
          </div>
        </div>
        <ActionButton
          type="submit"
          actionRole="secondary"
          loading={busy}
          disabled={disabled || (!nameDirty && !aliasDirty)}
        >
          Save
        </ActionButton>
      </form>
    </details>
  );
}
